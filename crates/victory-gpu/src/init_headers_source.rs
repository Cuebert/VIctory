//! Observe the real InitHeaders argument records under explicit backing ownership.
use crate::host::HostList;
use std::sync::Arc;
use crate::list::CommandUpload;

pub(crate) struct HeldRecords {
    upload: Arc<CommandUpload>,
    source: u64,
    len: usize,
    recorded: u64,
    #[cfg(test)]
    observed: Option<Arc<std::sync::Mutex<Vec<u8>>>>,
}

impl HeldRecords {
    pub(crate) fn submitted(&self, frame: u64, fence: u64, list: usize) {
        if !crate::capture::listed(frame) { return; }
        if let Some(data) = self.upload.snapshot(self.source as usize, self.len) {
            #[cfg(test)]
            if let Some(sink) = &self.observed { *sink.lock().unwrap() = data.clone(); }
            tracing::info!(frame,recorded=self.recorded,fence,list,source=format_args!("{:#x}",self.source),
                bytes=self.len,nonzero=data.iter().filter(|&&b|b!=0).count(),
                "original InitHeaders upload after queue preparation");
            records(frame,self.source,&data,"same command list's retained upload before native execution");
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(upload: Arc<CommandUpload>, source: u64, len: usize,
        seen: Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
        Self{upload,source,len,recorded:1,observed:Some(seen)}
    }
}

pub(super) fn record(list:&HostList, source:u64, count:Option<u32>, groups:[u32;3],
    upload:impl FnOnce(usize, usize)->Option<Arc<CommandUpload>>) {
    let frame=crate::queue::presented()+1;
    if !crate::capture::trace_window(frame) {return;}
    static BUDGET:std::sync::Mutex<(u64,u8)>=std::sync::Mutex::new((0,0));
    let mut budget=BUDGET.lock().unwrap_or_else(|e|e.into_inner());
    if frame>budget.0{*budget=(frame,0);}
    if budget.0!=frame || budget.1>=2{return;}budget.1+=1;drop(budget);
    let count=count.unwrap_or(0);
    let n=if groups.contains(&0){0}else{count.min(groups[0].saturating_mul(32)).min(4)};
    let bytes=u64::from(n)*40;
    let owner=crate::shadow::range_evidence(source,bytes);
    tracing::info!(frame,address=format_args!("{source:#x}"),?groups,count,bytes,%owner,
        "InitHeaders source at original dispatch");
    if bytes==0{return;}
    if let Some(upload)=upload(source as usize,bytes as usize) {
        if let Some(data)=upload.snapshot(source as usize,bytes as usize) {
            records(frame,source,&data,"same command list's live upload at recording");
        }
        list.retain_init_header_records(HeldRecords {upload,source,len:bytes as usize,recorded:frame,
            #[cfg(test)]
            observed:None});
    } else if let Some(s)=crate::shadow::owned_compute_span(source,bytes,true,false) {
        let Some(host)=crate::host::host() else{return;};
        s.refresh(list,source,bytes);
        list.buffer_state(&s.buffer,crate::host::BUFFER_READ);
        use crate::capture::buffer_sample::{Binding,SampleLocation,record_copy};
        record_copy(list,&host.device,&s.buffer,s.offset(source),SampleLocation {
            binding:Binding{hash:0x3cea4534bbc15632,kind:victory_shader::ResourceType::Srv,
                slot:0,offset:0,whole:true},frame,address:source,size:bytes});
    } else if let Some(read)=crate::resource::TEXTURE_PAGE_READER.get().copied() {
        // The physical-section reader copies through its own retained section
        // views. No read of the title VA or of an observed reservation occurs.
        list.defer_host(move|| {
            let frame=crate::queue::presented()+1;
            if !crate::capture::listed(frame){return;}
            let mut data=vec![0;bytes as usize];
            let ranges=read(source as usize,&mut data);
            if ranges.as_slice()==[(0,data.len())] {
                records(frame,source,&data,"retained physical section at submission");
            }
        });
    }
}

pub(crate) fn records(frame:u64,source:u64,data:&[u8],backing:&str){
    for (i,record) in data.chunks_exact(40).take(4).enumerate() {
        let u32_at=|at:usize|u32::from_le_bytes(record[at..at+4].try_into().unwrap());
        let u64_at=|at:usize|u64::from_le_bytes(record[at..at+8].try_into().unwrap());
        let header=u64_at(0);let centroid=u64_at(8);
        let fields:[u32;6]=std::array::from_fn(|j|u32_at(16+4*j));
        let header_owner=crate::shadow::range_evidence(header,64);
        let centroid_owner=crate::shadow::range_evidence(centroid,24);
        tracing::info!(frame,source=format_args!("{source:#x}"),record=i,backing,
            header=format_args!("{header:#x}"),centroid=format_args!("{centroid:#x}"),?fields,
            %header_owner,%centroid_owner,"original InitHeaders argument record");
    }
}
