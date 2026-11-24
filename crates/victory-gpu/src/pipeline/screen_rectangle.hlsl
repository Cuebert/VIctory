struct Vertex { float4 position : SV_Position; };

// A screen rectangle spans the bounding box of the three supplied corners.
// The shared diagonal and the top-left rasterization rule cover each sample once.
[maxvertexcount(4)]
void main(triangle Vertex v[3], inout TriangleStream<Vertex> stream) {
    float4 a = v[0].position, b = v[1].position, c = v[2].position;
    if (a.w == 0 || b.w == 0 || c.w == 0) return;
    float2 p = a.xy / a.w, q = b.xy / b.w, r = c.xy / c.w;
    float2 lo = min(p, min(q, r)), hi = max(p, max(q, r));
    float2 u = q - p, t = r - p;
    float det = u.x * t.y - u.y * t.x;
    if (det == 0) return;
    [unroll] for (uint i = 0; i < 4; ++i) {
        float2 xy = float2((i & 1) ? hi.x : lo.x, (i & 2) ? lo.y : hi.y);
        float2 delta = xy - p;
        float l = (delta.x * t.y - delta.y * t.x) / det;
        float m = (u.x * delta.y - u.y * delta.x) / det;
        // Depth and reciprocal clip W are affine in screen space.
        float2 za = float2(a.z, 1) / a.w;
        float2 zw = za + l * (float2(b.z, 1) / b.w - za) + m * (float2(c.z, 1) / c.w - za);
        Vertex result;
        result.position = float4(xy, zw.x, 1) / zw.y;
        stream.Append(result);
    }
}
