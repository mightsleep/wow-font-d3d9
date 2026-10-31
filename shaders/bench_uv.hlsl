// The bench harness's probe (examples/bench.rs): where each pixel samples,
// as the decoder sees it: p in texels and the texels a pixel, fw, both as
// the decoder computes them. The exact reference is evaluated there.
float4 main(float4 color : COLOR0, float2 uv : TEXCOORD0) : COLOR0
{
    float2 p = uv * 256.0;
    float2 fw = max(abs(ddx(p)) + abs(ddy(p)), 1.0 / 64.0);
    return float4(p, fw);
}
