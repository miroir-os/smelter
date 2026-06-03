# VAAPI DMA-BUF zero-copy notes

This branch keeps the zero-copy path split at the ownership boundaries that matter for
review:

- `smelter-core` owns VAAPI decode/encode lifetime with local H264 VAAPI code.
- `smelter-render` owns WGPU DMA-BUF import/export and render-output frame pooling.
- `DmaBufFrame` is the render-side frame contract. Callers get metadata through methods
  and do not reach into DMA-BUF object/layer storage directly.
- Decoder output imports each VA surface into WGPU once and caches by `VASurfaceID`.
- Renderer output reuses a ring of NV12 DMA-BUF frames and their WGPU views.
- The VAAPI H264 encoder provides VA-owned renderable NV12 DMA-BUF frames, so the
  compositor renders directly into encoder input surfaces instead of round-tripping
  through Vulkan-owned export surfaces.
- The render-output ring is normal render-graph state, not shared concurrent state.

The old cros-codecs dependency and vendored patch tree are gone. The remaining
dependency is `cros-libva`, used as the VA binding for typed buffers and safe surface
lifetime where it fits. Raw VA calls stay isolated in `pipeline::vaapi` for the one
operation the wrapper does not expose in the shape we need: exporting VA-owned encoder
input surfaces with write access and a retained owner.

Bench reference on `miroir-bench` after the output pool, import-cache, and render
pool cleanup:

```text
3000 frames, VAAPI H264 MP4 input, no readback/write:
942.03 fps, gap p99 7.362 ms, max 16.098 ms
```
