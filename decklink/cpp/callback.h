#pragma once

#include "api.h"
#include <atomic>
#include <cstdint>
#include <cstring>
#include <cstdio>
#include <ctime>

template <typename Interface, const REFIID &Iid>
class ComObject : public Interface {
private:
  std::atomic<ULONG> refcount = 1;

public:
  virtual HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, LPVOID *out) {
    REFIID unknown = IID_IUnknown;
    if (memcmp(&iid, &Iid, sizeof(REFIID)) != 0 &&
        memcmp(&iid, &unknown, sizeof(REFIID)) != 0) {
      *out = nullptr;
      return E_NOINTERFACE;
    }
    AddRef();
    *out = static_cast<Interface *>(this);
    return S_OK;
  }

  virtual ULONG STDMETHODCALLTYPE AddRef(void) { return ++refcount; }

  virtual ULONG STDMETHODCALLTYPE Release(void) {
    ULONG remaining = --refcount;
    if (remaining == 0) {
      delete this;
    }
    return remaining;
  }

protected:
  virtual ~ComObject() = default;
};

class InputCallbackWrapper
    : public ComObject<IDeckLinkInputCallback, IID_IDeckLinkInputCallback> {
private:
  rust::Box<DynInputCallback> cb;

public:
  InputCallbackWrapper(rust::Box<DynInputCallback> cb) : cb(std::move(cb)){};

  virtual HRESULT STDMETHODCALLTYPE
  VideoInputFrameArrived(IDeckLinkVideoInputFrame *video_frame,
                         IDeckLinkAudioInputPacket *audio_packet);
  virtual HRESULT STDMETHODCALLTYPE
  VideoInputFormatChanged(BMDVideoInputFormatChangedEvents events,
                          IDeckLinkDisplayMode *display_mode,
                          BMDDetectedVideoInputFormatFlags flags);
};

class FrameAllocatorProvider
    : public ComObject<IDeckLinkVideoBufferAllocatorProvider,
                       IID_IDeckLinkVideoBufferAllocatorProvider> {
public:
  rust::Box<DynFrameAllocator> allocator;

  FrameAllocatorProvider(rust::Box<DynFrameAllocator> allocator)
      : allocator(std::move(allocator)){};

  HRESULT GetVideoBufferAllocator(uint32_t buffer_size, uint32_t, uint32_t,
                                  uint32_t row_bytes, BMDPixelFormat,
                                  IDeckLinkVideoBufferAllocator **out) override;
};

class FrameBuffer
    : public ComObject<IDeckLinkVideoBuffer, IID_IDeckLinkVideoBuffer> {
private:
  FrameAllocatorProvider *provider;
  uint8_t *bytes;

public:
  FrameBuffer(FrameAllocatorProvider *provider, uint8_t *bytes)
      : provider(provider), bytes(bytes) {
    provider->AddRef();
  }

  ~FrameBuffer() {
    provider->allocator->release(bytes);
    provider->Release();
  }

  HRESULT GetBytes(void **out) override {
    *out = bytes;
    return S_OK;
  }
  static void probe(const char *what, void *bytes, BMDBufferAccessFlags flags) {
    static std::atomic<int> lines = 0;
    if (lines++ > 4000) return;
    timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    if (FILE *log = fopen("/tmp/zc-access.log", "a")) {
      fprintf(log, "%lld %s %p %u\n", (long long)now.tv_sec * 1000000000LL + now.tv_nsec, what, bytes, (unsigned)flags);
      fclose(log);
    }
  }
  HRESULT StartAccess(BMDBufferAccessFlags flags) override { probe("start", bytes, flags); return S_OK; }
  HRESULT EndAccess(BMDBufferAccessFlags flags) override { probe("end", bytes, flags); return S_OK; }
};

class FrameBufferAllocator
    : public ComObject<IDeckLinkVideoBufferAllocator,
                       IID_IDeckLinkVideoBufferAllocator> {
private:
  FrameAllocatorProvider *provider;
  uint32_t buffer_size;
  uint32_t row_bytes;

public:
  FrameBufferAllocator(FrameAllocatorProvider *provider, uint32_t buffer_size,
                       uint32_t row_bytes)
      : provider(provider), buffer_size(buffer_size), row_bytes(row_bytes) {
    provider->AddRef();
  }

  ~FrameBufferAllocator() { provider->Release(); }

  HRESULT AllocateVideoBuffer(IDeckLinkVideoBuffer **out) override {
    uint8_t *bytes = provider->allocator->allocate(buffer_size, row_bytes);
    if (bytes == nullptr) {
      return E_OUTOFMEMORY;
    }
    *out = new FrameBuffer(provider, bytes);
    return S_OK;
  }
};

inline HRESULT FrameAllocatorProvider::GetVideoBufferAllocator(
    uint32_t buffer_size, uint32_t, uint32_t, uint32_t row_bytes,
    BMDPixelFormat, IDeckLinkVideoBufferAllocator **out) {
  *out = new FrameBufferAllocator(this, buffer_size, row_bytes);
  return S_OK;
}
