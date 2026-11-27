// SPDX-License-Identifier: GPL-3.0-or-later
// Output setter and scale sites adapted from Belobug/VIctory-Belobug-AMD
// 4eb6cfdf9e61cb112f1f811344ec0dd309eddd6c, victory-loader/src/resolution.rs.
use anyhow::{Context, Result, ensure};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::LoadedImage;

/// A place to change: its offset in the image and the bytes there
/// (compared first; the replacement is at most as long).
pub struct Site {
    pub at: usize,
    pub what: &'static str,
    pub bytes: &'static [u8],
}

/// The screen size setter, its screen height, and what +0x22434a0 calls when
/// the height changed.
pub const SET_SCREEN_SIZE: usize = 0x2478130;
pub const SCREEN_HEIGHT: usize = 0xa9438d0;
pub const HEIGHT_CHANGED: usize = 0xd8c3a0;

/// +0x22434a0 from +0x224352b to +0x2243568: the settings' size handed to
/// the screen size.
pub const OUTPUT: Site = Site {
    at: 0x224352b,
    what: "settings' output size applied",
    bytes: &[
        0x8b, 0x0d, 0x9b, 0x03, 0x70, 0x08, // mov ecx, [+0xa9438cc]
        0x8b, 0x86, 0x04, 0x02, 0x00, 0x00, // mov eax, [rsi+204h]
        0x39, 0xc1, 0x75, 0x0d, // cmp ecx, eax ; jne
        0x8b, 0x96, 0x08, 0x02, 0x00, 0x00, // mov edx, [rsi+208h]
        0xe8, 0xea, 0x4b, 0x23, 0x00, 0xeb, 0x20, // call +0x2478130 ; jmp +0x2243568
        0x8b, 0xbe, 0x08, 0x02, 0x00, 0x00, // mov edi, [rsi+208h]
        0x8b, 0x2d, 0x7c, 0x03, 0x70, 0x08, // mov ebp, [+0xa9438d0]
        0x89, 0xc1, 0x89, 0xfa, // mov ecx, eax ; mov edx, edi
        0xe8, 0xd3, 0x4b, 0x23, 0x00, // call +0x2478130
        0x39, 0xfd, 0x74, 0x07, 0xb1, 0x01, // cmp ebp, edi ; je ; mov cl, 1
        0xe8, 0x38, 0x8e, 0xb4, 0xfe, // call +0xd8c3a0
    ],
};

/// +0x247ad10, after the back buffers are made in their pool (64 MB on
/// Series X, 82 MB on Series S, from +0x247ad45): `int3` unless less than
/// 2 MB of the pool is left over. Sized for 3840x2160; at 1920x1080 49 MB
/// are left (run res-1 stopped here). With another output size the check
/// becomes a jump over the `int3`; the pool stays as large.
pub const POOL_CHECK: Site = Site {
    at: 0x247af25,
    what: "back buffer pool's left-over check",
    bytes: &[0x48, 0x3d, 0xff, 0xff, 0x1f, 0x00, 0x76, 0x01, 0xcc], // cmp rax, 1FFFFFh ; jbe +1 ; int3
};

/// What goes at [`POOL_CHECK`]: `jmp +1` in place of `jbe +1`.
pub fn pool_check_code() -> Vec<u8> {
    let mut c = POOL_CHECK.bytes[..8].to_vec();
    c[6] = 0xeb;
    c
}

/// The title's own upscaler, its method +0x28f1e70: `mov dword [rcx+90h],
/// 2/3` (the scale +0x7e9440 renders at).
pub const OWN_SCALE: Site = Site {
    at: 0x28f1e78,
    what: "the title's own upscaler's render scale",
    bytes: &[0xc7, 0x81, 0x90, 0x00, 0x00, 0x00, 0xab, 0xaa, 0x2a, 0x3f],
};

/// `rel32` from the end of an instruction at image offset `next` to `to`.
fn rel(to: usize, next: usize) -> [u8; 4] {
    ((to as i64 - next as i64) as i32).to_le_bytes()
}

/// What goes at [`OUTPUT`] for `width` x `height` (the same length).
pub fn output_code(width: u32, height: u32) -> Vec<u8> {
    let at = OUTPUT.at;
    let mut c = Vec::with_capacity(OUTPUT.bytes.len());
    c.push(0xb8); // mov eax, width
    c.extend(width.to_le_bytes());
    c.extend([0x89, 0x86, 0x04, 0x02, 0x00, 0x00]); // mov [rsi+204h], eax
    c.push(0xbf); // mov edi, height
    c.extend(height.to_le_bytes());
    c.extend([0x89, 0xbe, 0x08, 0x02, 0x00, 0x00]); // mov [rsi+208h], edi
    c.extend([0x8b, 0x2d]); // mov ebp, [screen height]
    c.extend(rel(SCREEN_HEIGHT, at + c.len() + 4));
    c.extend([0x89, 0xc1, 0x89, 0xfa]); // mov ecx, eax ; mov edx, edi
    c.push(0xe8); // call the setter
    c.extend(rel(SET_SCREEN_SIZE, at + c.len() + 4));
    let end = OUTPUT.bytes.len();
    c.extend([0x39, 0xfd, 0x74, (end - (c.len() + 4)) as u8]); // cmp ebp, edi ; je end
    c.extend([0xb1, 0x01, 0xe8]); // mov cl, 1 ; call
    c.extend(rel(HEIGHT_CHANGED, at + c.len() + 4));
    c.extend([0xeb, (end - (c.len() + 2)) as u8]); // jmp end
    c.resize(end, 0xcc);
    c
}

/// What goes at [`OWN_SCALE`] for `scale`.
pub fn own_scale_code(scale: f32) -> Vec<u8> {
    let mut c = OWN_SCALE.bytes[..6].to_vec();
    c.extend(scale.to_le_bytes());
    c
}


/// Preflight every site before writing any bytes. The caller owns the mapped
/// main image and has not started any title code.
pub unsafe fn apply(image: &LoadedImage, original_hash: &str, execute: bool) -> Result<()> {
    ensure!(
        original_hash == "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93",
        "1080p-to-1440p requires the exact reviewed Series X image"
    );
    let patches = [
        (&OUTPUT, output_code(2560, 1440)),
        (&POOL_CHECK, pool_check_code()),
        (&OWN_SCALE, own_scale_code(0.75)),
    ];
    let size = image.info().size_of_image as usize;
    for (site, code) in &patches {
        ensure!(code.len() <= site.bytes.len() && site.at + site.bytes.len() <= size,
            "resolution site outside the image: {}", site.what);
        let current = unsafe {
            std::slice::from_raw_parts((image.base() + site.at) as *const u8, site.bytes.len())
        };
        ensure!(current == site.bytes, "resolution site changed: {}", site.what);
    }
    if execute {
        for (site, code) in patches {
            let address = (image.base() + site.at) as *mut u8;
            let mut old = 0;
            unsafe {
                if VirtualProtect(address.cast(), code.len(), PAGE_READWRITE, &mut old) == 0 {
                    return Err(std::io::Error::last_os_error()).context(site.what);
                }
                std::ptr::copy_nonoverlapping(code.as_ptr(), address, code.len());
                let mut previous = 0;
                if VirtualProtect(address.cast(), code.len(), old, &mut previous) == 0
                    || FlushInstructionCache(GetCurrentProcess(), address.cast(), code.len()) == 0
                {
                    return Err(std::io::Error::last_os_error()).context(site.what);
                }
            }
        }
    }
    let phase = if execute { "applied" } else { "validated-only" };
    println!("fox-1080-1440 phase={phase} output=2560x1440 own_upscaler_scale=0.75 target_scene=1920x1080");
    Ok(())
}