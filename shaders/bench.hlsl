// The bench harness's vertex shader (examples/bench.rs): positions come
// in clip space, colour and texture coordinate go through, like the
// client's text shader as far as the decoder can tell.
struct V {
    float4 pos : POSITION;
    float4 color : COLOR0;
    float2 uv : TEXCOORD0;
};

V main(V v)
{
    return v;
}
