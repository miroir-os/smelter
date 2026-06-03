# VAAPI DMA-BUF zero-copy notes

This branch keeps the zero-copy path split at the ownership boundaries that matter for
review:

- `smelter-core` owns VAAPI decode/encode lifetime and cros-codecs interop.
- `smelter-render` owns WGPU DMA-BUF import/export and render-output frame pooling.
- `DmaBufFrame` is the render-side frame contract. Callers get metadata through methods
  and do not reach into DMA-BUF object/layer storage directly.
- Decoder output imports each VA surface into WGPU once and caches by `VASurfaceID`.
- Renderer output reuses a ring of exported NV12 DMA-BUF frames and their WGPU views.
- The render-output ring is normal render-graph state, not shared concurrent state.

The vendored cros-codecs delta is intentionally narrow but cannot currently be reduced
to a decoder-only `Borrow<Surface<_>>` bound without a larger C2 encoder API change.
The VAAPI decoder only needs to borrow native handles so decoded pooled surfaces can stay
attached to their pool, but cros-codecs' generic C2 VAAPI encoder constructors also need
to turn `VideoFrame::NativeHandle` into a raw `Surface<_>`.

Two smaller experiments were rejected by Linux builds:

- Moving `Into<Surface<_>>` from `VideoFrame::NativeHandle` only to
  `StatelessEncoderBackendImport` leaves the C2 VAAPI encoder constructors without a
  provable bound.
- Hiding the bound behind a C2 marker trait still does not give generic encoder call
  sites enough associated-type evidence without spreading the same bound through the C2
  worker API.

For now, the upstreamable cros-codecs patch shape is:

- Store VAAPI decoder pictures as `V::NativeHandle` instead of detaching them into
  `Surface<_>`.
- Expose decoded VA surface ID and DRM PRIME export on `VaapiDecodedHandle`.
- Replace the local `Equivalent<Surface<_>>` helper with standard `Borrow<Surface<_>> +
  Into<Surface<_>>`.
- Add `From<PooledVaSurface<_>> for Surface<_>` for the existing encoder path that needs
  consumption.

Bench reference on `miroir-bench` after the output pool, import-cache, and render
pool cleanup:

```text
3000 frames, VAAPI H264 MP4 input, no readback/write:
942.03 fps, gap p99 7.362 ms, max 16.098 ms
```
