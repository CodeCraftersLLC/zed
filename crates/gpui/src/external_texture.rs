//! Caller-owned GPU textures composited **inside** the GPUI scene.
//!
//! This exists for one reason: Cherry Pick's embedded browser paints Chromium
//! off-screen and needs those pixels inside the swap chain, at 60fps, without
//! going anywhere near the sprite atlas.
//!
//! # Why not the sprite atlas
//!
//! [`crate::Window::paint_image`] packs its pixels into a shared atlas texture
//! alongside every icon and glyph on screen. That is right for content that is
//! uploaded once and drawn many times. A live web page is the opposite: a
//! 1920x1080 BGRA frame is 8 MB, it changes on its own schedule, and pushing it
//! through the atlas would evict every icon in the app on the first frame and
//! then thrash the allocator forever after. It would also serialise browser
//! uploads behind unrelated text rendering.
//!
//! So an external texture gets its own GPU texture, keyed by [`ExternalTextureId`],
//! that the renderer creates once and re-uses. Only the pixels that changed are
//! re-uploaded, and a page that has settled uploads nothing at all.
//!
//! # Threading
//!
//! The producer (a Chromium OSR paint callback, on a Chromium thread) writes
//! into its own buffer and bumps [`ExternalFrameView::sequence`]. The consumer
//! (a GPUI renderer, on the UI thread during paint) calls
//! [`ExternalTextureSource::with_frame`]. [`ExternalTextureBuffer`] publishes
//! each frame as an immutable snapshot, so the visit — which uploads to the GPU
//! — runs without any producer lock held and a submit never waits for a render.
//! A source that does take a lock in `with_frame` must keep that critical
//! section to a memcpy's worth of work; it runs inside the frame budget.
//!
//! # Resize
//!
//! Reallocating the buffer bumps [`ExternalFrameView::generation`]. A renderer
//! that sees a new generation throws its cached texture away and creates a new
//! one, which is what keeps a stale-sized texture from being sampled with new
//! dimensions after a pane resize.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Bounds, DevicePixels, Size};

/// Stable identity for one external texture across frames.
///
/// The renderer caches a GPU texture per id, so this must be stable for the
/// life of the producing surface and must never be reused by a different
/// producer while the old one could still be in a scene.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExternalTextureId(pub u64);

impl ExternalTextureId {
    /// Hand out an id no other caller in this process will get.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Byte order of an external frame's pixels.
///
/// Both are 8 bits per channel and 4 bytes per pixel; only the channel order
/// differs. Chromium OSR hands out `Bgra8`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExternalTextureFormat {
    /// Blue, green, red, alpha. What Chromium's `OnPaint` produces.
    Bgra8,
    /// Red, green, blue, alpha.
    Rgba8,
}

impl ExternalTextureFormat {
    /// Bytes per pixel. Both variants are 4.
    pub const fn bytes_per_pixel(self) -> usize {
        4
    }
}

/// A borrowed look at the producer's current pixels.
///
/// Only valid for the duration of the [`ExternalTextureSource::with_frame`]
/// callback that produced it.
pub struct ExternalFrameView<'a> {
    /// Pixel dimensions of the whole buffer.
    pub size: Size<DevicePixels>,
    /// Bytes between the start of one row and the start of the next. May be
    /// larger than `size.width * 4` when the producer pads rows.
    pub stride: usize,
    /// Channel order of `bytes`.
    pub format: ExternalTextureFormat,
    /// The pixels. At least `stride * size.height` long.
    pub bytes: &'a [u8],
    /// Bumped whenever the buffer is reallocated, which for a browser means a
    /// resize. A renderer must recreate its cached texture when this changes.
    pub generation: u64,
    /// Bumped on every painted frame. A renderer that has already uploaded this
    /// sequence must not upload again — this is what makes a settled page cost
    /// zero bandwidth.
    pub sequence: u64,
    /// The regions that changed since the oldest frame a renderer still needs,
    /// in texture pixels: a union, not just this frame's changes, because a
    /// renderer that missed a frame needs everything that moved while it was
    /// away. Empty means "assume everything changed", which is correct but
    /// expensive; a producer should always fill this in when it knows.
    pub dirty: &'a [Bounds<DevicePixels>],
}

impl ExternalFrameView<'_> {
    /// Whether the buffer is large enough for its own declared geometry.
    ///
    /// A renderer must check this before indexing. A producer that gets this
    /// wrong is a bug, but it must not be a GPU crash or an out-of-bounds read.
    pub fn is_well_formed(&self) -> bool {
        let width = self.size.width.0.max(0) as usize;
        let height = self.size.height.0.max(0) as usize;
        if width == 0 || height == 0 {
            return false;
        }
        let Some(row_bytes) = width.checked_mul(self.format.bytes_per_pixel()) else {
            return false;
        };
        // Both native upload APIs represent the row pitch as u32.
        self.stride >= row_bytes
            && u32::try_from(self.stride).is_ok()
            && self
                .stride
                .checked_mul(height)
                .is_some_and(|needed| needed <= self.bytes.len())
    }

    /// The dirty regions, clamped to the buffer, with an empty list meaning the
    /// whole frame. Renderers should upload exactly these.
    pub fn dirty_regions(&self) -> Vec<Bounds<DevicePixels>> {
        let full = Bounds {
            origin: crate::point(DevicePixels(0), DevicePixels(0)),
            size: self.size,
        };
        if self.dirty.is_empty() {
            return vec![full];
        }
        let regions: Vec<_> = self
            .dirty
            .iter()
            .filter_map(|rect| {
                // External coordinates are untrusted. Widen before adding so
                // malformed i32 endpoints cannot wrap into a native GPU box.
                if rect.size.width.0 <= 0 || rect.size.height.0 <= 0 {
                    return None;
                }
                let left = i64::from(rect.origin.x.0).clamp(0, i64::from(self.size.width.0.max(0)));
                let top = i64::from(rect.origin.y.0).clamp(0, i64::from(self.size.height.0.max(0)));
                let right = (i64::from(rect.origin.x.0) + i64::from(rect.size.width.0))
                    .clamp(left, i64::from(self.size.width.0.max(0)));
                let bottom = (i64::from(rect.origin.y.0) + i64::from(rect.size.height.0))
                    .clamp(top, i64::from(self.size.height.0.max(0)));
                (right > left && bottom > top).then_some(Bounds {
                    origin: crate::point(DevicePixels(left as i32), DevicePixels(top as i32)),
                    size: crate::size(
                        DevicePixels((right - left) as i32),
                        DevicePixels((bottom - top) as i32),
                    ),
                })
            })
            .collect();
        // Dirty rectangles are hints about a complete frame. If every hint
        // falls outside the current texture, acknowledging the sequence with
        // no upload would leave stale pixels cached indefinitely.
        if regions.is_empty() {
            vec![full]
        } else {
            regions
        }
    }
}

/// Something that produces frames for an external texture.
///
/// Implemented outside GPUI (Cherry Pick's browser engine implements it over a
/// Chromium OSR buffer). GPUI only reads.
pub trait ExternalTextureSource: fmt::Debug + Send + Sync + 'static {
    /// Stable identity, so the renderer can cache one GPU texture per source.
    fn id(&self) -> ExternalTextureId;

    /// Show the renderer the current frame, if there is one.
    ///
    /// Called on the UI thread inside paint. `visit` is not called when no
    /// frame has been produced yet, which is the normal state for a pane whose
    /// page has not painted.
    fn with_frame(&self, visit: &mut dyn FnMut(ExternalFrameView<'_>));

    /// Told to the source after the renderer uploads `sequence`.
    ///
    /// Sources use it for the upload counter the performance gates read
    /// (PERF-B04: a static page must settle at zero uploads per second).
    ///
    /// **Called from inside [`Self::with_frame`]**, because that is the only
    /// place a renderer can see the sequence it just uploaded. An
    /// implementation must therefore not take any lock that `with_frame`
    /// holds, or the render thread deadlocks on the first painted frame.
    ///
    /// This is the single-consumer shorthand, and it counts as an
    /// acknowledgement from [`ExternalTextureConsumerId::LEGACY`]. A renderer
    /// that may share the source with another renderer should call
    /// [`Self::mark_uploaded_for`] with its own consumer id instead.
    fn mark_uploaded(&self, sequence: u64);

    /// Told to the source after `consumer` uploads `sequence`.
    ///
    /// The default forwards to [`Self::mark_uploaded`], which is all a source
    /// with one consumer needs. [`ExternalTextureBuffer`] overrides it so dirty
    /// regions are kept until *every* renderer that has drawn the source has
    /// caught up, because one renderer acknowledging a frame must not make a
    /// second renderer's texture stale.
    fn mark_uploaded_for(&self, _consumer: ExternalTextureConsumerId, sequence: u64) {
        self.mark_uploaded(sequence);
    }

    /// The renderer behind `consumer` no longer draws this source.
    ///
    /// Called when a renderer drops the texture it cached for this source — a
    /// closed window, a GPU device loss, or a cache prune. A source that keeps
    /// per-consumer state must forget the consumer, or a renderer that will
    /// never upload again would hold dirty regions for the rest of the source's
    /// life. The default does nothing, for sources that keep no such state.
    fn remove_consumer(&self, _consumer: ExternalTextureConsumerId) {}

    /// How many uploads this source has served. Instrumentation only.
    fn upload_count(&self) -> u64;
}

/// Identity of one renderer drawing a source's frames.
///
/// A source can be drawn by more than one renderer — the same pane shown in two
/// windows — and each renderer uploads on its own schedule. Acknowledgements are
/// per consumer so that the renderer which draws second is not left with stale
/// pixels because the renderer which drew first acknowledged the frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExternalTextureConsumerId(pub u64);

impl ExternalTextureConsumerId {
    /// The slot [`ExternalTextureSource::mark_uploaded`] acknowledges, for
    /// sources and callers that only ever have one consumer.
    pub const LEGACY: Self = Self(0);

    /// Hand out an id no other renderer in this process will get.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A frame producer plus the bookkeeping every implementation would otherwise
/// duplicate: double buffering, dirty-rect accumulation, sequence and
/// generation counters, and the upload counter the perf gates read.
///
/// Producers call [`Self::submit`]; GPUI calls the [`ExternalTextureSource`]
/// methods. Neither side needs to know about the other's thread.
pub struct ExternalTextureBuffer {
    id: ExternalTextureId,
    /// Producer-side bookkeeping. It holds no pixels, so a renderer never holds
    /// this lock while it uploads and a producer never waits for one.
    state: parking_lot::Mutex<BufferState>,
    /// The frame renderers see. `submit` swaps a fresh snapshot in and
    /// `with_frame` clones the `Arc` out before the visit, so a GPU upload
    /// cannot block the producer's next frame.
    frame: parking_lot::Mutex<Option<Arc<ExternalFrame>>>,
    /// The allocation the next snapshot reuses. A steady stream of same-sized
    /// frames therefore does not churn the allocator.
    spare: parking_lot::Mutex<Vec<u8>>,
    /// Read by perf assertions from another thread, so it lives outside every
    /// lock.
    uploads: AtomicU64,
    /// Mirrors `state.sequence` for lock-free reads.
    sequence: AtomicU64,
    /// Last uploaded sequence per renderer that has drawn this source.
    consumers: parking_lot::Mutex<std::collections::BTreeMap<ExternalTextureConsumerId, u64>>,
}

#[derive(Default)]
struct BufferState {
    size: Size<DevicePixels>,
    stride: usize,
    format: Option<ExternalTextureFormat>,
    generation: u64,
    sequence: u64,
    dirty: Vec<Bounds<DevicePixels>>,
}

/// The pixels and metadata of one published frame, shared with whichever
/// renderers are drawing the source.
struct ExternalFrame {
    bytes: Vec<u8>,
    size: Size<DevicePixels>,
    stride: usize,
    format: ExternalTextureFormat,
    generation: u64,
    sequence: u64,
    dirty: Vec<Bounds<DevicePixels>>,
}

impl fmt::Debug for ExternalTextureBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        f.debug_struct("ExternalTextureBuffer")
            .field("id", &self.id)
            .field("size", &state.size)
            .field("generation", &state.generation)
            .field("sequence", &state.sequence)
            .finish()
    }
}

impl Default for ExternalTextureBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalTextureBuffer {
    /// A buffer with no frame yet. Allocates nothing until the first
    /// [`Self::submit`], which is what lets a browser pane exist without
    /// costing a framebuffer (PRD ENG-02).
    pub fn new() -> Self {
        Self {
            id: ExternalTextureId::next(),
            state: parking_lot::Mutex::new(BufferState::default()),
            frame: parking_lot::Mutex::new(None),
            spare: parking_lot::Mutex::new(Vec::new()),
            uploads: AtomicU64::new(0),
            sequence: AtomicU64::new(0),
            consumers: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Wrap in an `Arc` for handing to [`crate::Window::paint_external_texture`].
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Whether any frame has been produced. A pane whose page has not painted
    /// yet must not draw a black rectangle over its own placeholder.
    pub fn has_frame(&self) -> bool {
        self.sequence.load(Ordering::Acquire) > 0
    }

    /// The frame counter, for tests and instrumentation.
    pub fn sequence(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    /// Last frame committed by any renderer. CPU submissions do not advance it.
    pub fn uploaded_sequence(&self) -> u64 {
        self.consumers.lock().values().copied().max().unwrap_or(0)
    }

    /// Current buffer dimensions, or zero before the first frame.
    pub fn size(&self) -> Size<DevicePixels> {
        self.state.lock().size
    }

    /// Publish a frame.
    ///
    /// `dirty` is in texture pixels; an empty slice means the whole frame. A
    /// size or format change reallocates and bumps the generation, and the
    /// dirty list is then irrelevant because the renderer will recreate the
    /// texture anyway.
    ///
    /// Returns `false` and drops the frame when `bytes` is too small for the
    /// declared geometry, so a misbehaving producer cannot make the renderer
    /// read out of bounds.
    pub fn submit(
        &self,
        size: Size<DevicePixels>,
        stride: usize,
        format: ExternalTextureFormat,
        bytes: &[u8],
        dirty: &[Bounds<DevicePixels>],
    ) -> bool {
        let width = size.width.0.max(0) as usize;
        let height = size.height.0.max(0) as usize;
        if width == 0
            || height == 0
            || u32::try_from(stride).is_err()
            || width
                .checked_mul(format.bytes_per_pixel())
                .is_none_or(|minimum| stride < minimum)
        {
            return false;
        }
        let needed = stride
            .checked_mul(height)
            .filter(|needed| *needed <= bytes.len());
        let Some(needed) = needed else {
            return false;
        };

        // Bookkeeping and publication share one critical section, so two
        // producers cannot publish frames out of sequence order. The lock holds
        // a frame-sized memcpy, exactly as it did before the pixels moved into
        // a snapshot — but it is *not* held across a visit, which is the part
        // that used to stall a producer behind a GPU upload.
        let mut state = self.state.lock();
        let reallocated =
            state.size != size || state.stride != stride || state.format != Some(format);
        if reallocated {
            state.size = size;
            state.stride = stride;
            state.format = Some(format);
            state.generation += 1;
            // A recreated texture is uploaded whole; per-rect bookkeeping
            // would just be thrown away.
            state.dirty.clear();
        } else if self.has_unconsumed(&state) {
            // Some renderer has not seen the previous frame, so this frame's
            // dirt is added to it rather than replacing it. Dropping it would
            // leave half the page showing stale pixels.
            // Empty means a full repaint. It must dominate in either order;
            // appending a small rectangle to it would lose that full repaint.
            // Bound the list when a hidden or slow consumer misses many frames:
            // an empty list is a full upload, which is correct and only
            // expensive.
            if dirty.is_empty() || state.dirty.len().saturating_add(dirty.len()) > 64 {
                state.dirty.clear();
            } else if !state.dirty.is_empty() {
                state.dirty.extend_from_slice(dirty);
            }
        } else {
            // Every renderer that has drawn this source is caught up, so the
            // previous rects are spent.
            state.dirty.clear();
            if dirty.len() <= 64 {
                state.dirty.extend_from_slice(dirty);
            }
        }

        state.sequence += 1;
        let sequence = state.sequence;

        let mut spare = self.spare.lock();
        let mut staged = std::mem::take(&mut *spare);
        // `resize` truncates a larger spare and zero-fills only the tail that
        // grows, so a clear() first would memset every byte the copy is about
        // to overwrite — 8 MB per 1080p frame, inside the producer's lock.
        staged.resize(needed, 0);
        staged.copy_from_slice(&bytes[..needed]);
        let previous = self.frame.lock().replace(Arc::new(ExternalFrame {
            bytes: staged,
            size,
            stride,
            format,
            generation: state.generation,
            sequence,
            dirty: state.dirty.clone(),
        }));
        // Reuse the previous snapshot's allocation when no renderer is still
        // looking at it, so a steady stream of frames settles at zero
        // allocations.
        if let Some(previous) = previous
            && let Ok(previous) = Arc::try_unwrap(previous)
        {
            *spare = previous.bytes;
        }
        // Stored under the state lock: two producers must not write the mirror
        // out of order, and a concurrent `release_frame` must not leave it
        // ahead of a frame that no longer exists.
        self.sequence.store(sequence, Ordering::Release);
        drop(spare);
        drop(state);

        true
    }

    /// Whether any renderer that has drawn this source is behind the frame the
    /// pending dirty regions belong to.
    fn has_unconsumed(&self, state: &BufferState) -> bool {
        if state.sequence == 0 {
            return false;
        }
        let consumers = self.consumers.lock();
        consumers.values().any(|ack| *ack < state.sequence)
    }

    /// Record one renderer's upload. Lock-free from the producer's point of
    /// view, and safe from inside a visit: it takes no lock `with_frame` holds.
    fn acknowledge(&self, consumer: ExternalTextureConsumerId, sequence: u64) {
        self.consumers
            .lock()
            .entry(consumer)
            .and_modify(|ack| *ack = (*ack).max(sequence))
            .or_insert(sequence);
        self.uploads.fetch_add(1, Ordering::Relaxed);
    }

    /// Forget a renderer that has dropped this source's texture.
    ///
    /// Without this a renderer that will never draw again — a closed window, a
    /// recovered GPU device — would keep `has_unconsumed` true forever, and the
    /// dirty list would degrade to periodic full-frame uploads for every
    /// remaining consumer.
    pub fn remove_consumer(&self, consumer: ExternalTextureConsumerId) {
        self.consumers.lock().remove(&consumer);
    }

    /// Forget the frame without dropping the identity, so a hidden pane stops
    /// holding a framebuffer while keeping its renderer (PRD HID-01).
    pub fn release_frame(&self) {
        {
            let mut state = self.state.lock();
            state.size = Size::default();
            state.stride = 0;
            state.format = None;
            state.dirty.clear();
            state.generation += 1;
            state.sequence = 0;
        }
        *self.frame.lock() = None;
        // The spare is a framebuffer as well, and this call exists to stop a
        // hidden pane holding one (PRD HID-01), so it goes too.
        *self.spare.lock() = Vec::new();
        // A renderer that draws this source again starts from a recreate and
        // registers itself again on its first upload.
        self.consumers.lock().clear();
        self.sequence.store(0, Ordering::Release);
    }
}

impl ExternalTextureSource for ExternalTextureBuffer {
    fn id(&self) -> ExternalTextureId {
        self.id
    }

    fn with_frame(&self, visit: &mut dyn FnMut(ExternalFrameView<'_>)) {
        // Clone the snapshot out and let the lock go before the visit: the
        // visitor uploads to the GPU, and the producer's next frame must not
        // wait for that.
        let frame = self.frame.lock().clone();
        let Some(frame) = frame else {
            return;
        };
        visit(ExternalFrameView {
            size: frame.size,
            stride: frame.stride,
            format: frame.format,
            bytes: &frame.bytes,
            generation: frame.generation,
            sequence: frame.sequence,
            dirty: &frame.dirty,
        });
    }

    fn mark_uploaded(&self, sequence: u64) {
        self.acknowledge(ExternalTextureConsumerId::LEGACY, sequence);
    }

    fn mark_uploaded_for(&self, consumer: ExternalTextureConsumerId, sequence: u64) {
        self.acknowledge(consumer, sequence);
    }

    fn upload_count(&self) -> u64 {
        self.uploads.load(Ordering::Relaxed)
    }
}

/// What a renderer needs to decide whether to upload, kept out of the renderers
/// so Windows and Linux cannot drift apart on the rule.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ExternalTextureCacheKey {
    /// Which source the cached texture belongs to.
    pub id: ExternalTextureId,
    /// Which allocation. A change means recreate.
    pub generation: u64,
    /// Which frame. A change means upload the dirty rects.
    pub sequence: u64,
    /// Cached texture dimensions.
    pub size: Size<DevicePixels>,
}

/// The decision a renderer makes for one external texture this frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExternalTextureUpdate {
    /// No cached texture, or the producer reallocated. Create and upload whole.
    Recreate,
    /// Same texture, new pixels. Upload the dirty rects.
    UploadDirty,
    /// Nothing changed. Draw the cached texture and upload nothing. This is the
    /// branch a settled page must take every frame (PERF-B04).
    Reuse,
}

/// Decide what to do with a cached texture given the frame on offer.
pub fn plan_external_texture_update(
    cached: Option<ExternalTextureCacheKey>,
    frame: &ExternalFrameView<'_>,
) -> ExternalTextureUpdate {
    match cached {
        Some(cached)
            if cached.generation == frame.generation
                && cached.size == frame.size
                && cached.sequence == frame.sequence =>
        {
            ExternalTextureUpdate::Reuse
        }
        Some(cached) if cached.generation == frame.generation && cached.size == frame.size => {
            ExternalTextureUpdate::UploadDirty
        }
        _ => ExternalTextureUpdate::Recreate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{point, size};

    fn bgra(width: i32, height: i32, fill: u8) -> Vec<u8> {
        vec![fill; (width * height * 4) as usize]
    }

    fn dims(width: i32, height: i32) -> Size<DevicePixels> {
        size(DevicePixels(width), DevicePixels(height))
    }

    #[test]
    fn a_buffer_with_no_frame_never_calls_the_visitor() {
        let buffer = ExternalTextureBuffer::new();
        assert!(!buffer.has_frame());
        let mut seen = 0;
        buffer.with_frame(&mut |_| seen += 1);
        assert_eq!(seen, 0, "a pane that never painted must draw nothing");
    }

    /// The counter PERF-B04 reads must count uploads, not draws.
    ///
    /// This is the shape of a bug that shipped in the Windows renderer and not
    /// the wgpu one: it acknowledged whenever the cached sequence matched the
    /// frame's, which is exactly the `Reuse` condition, so a still page
    /// appeared to upload once per composited frame.
    #[test]
    fn reusing_a_cached_texture_is_not_an_upload() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(4, 2, 0x11);
        assert!(buffer.submit(dims(4, 2), 16, ExternalTextureFormat::Bgra8, &pixels, &[]));

        // First look at the frame: nothing cached, so the renderer uploads.
        let mut cached = None;
        buffer.with_frame(&mut |frame| {
            assert_eq!(
                plan_external_texture_update(cached, &frame),
                ExternalTextureUpdate::Recreate
            );
            cached = Some(ExternalTextureCacheKey {
                id: buffer.id(),
                generation: frame.generation,
                sequence: frame.sequence,
                size: frame.size,
            });
            buffer.mark_uploaded(frame.sequence);
        });
        assert_eq!(buffer.upload_count(), 1);

        // Every later frame draws the same pixels. A renderer that
        // acknowledges here is counting compositing, not uploading.
        for _ in 0..30 {
            buffer.with_frame(&mut |frame| {
                let plan = plan_external_texture_update(cached, &frame);
                assert_eq!(plan, ExternalTextureUpdate::Reuse);
                if !matches!(plan, ExternalTextureUpdate::Reuse) {
                    buffer.mark_uploaded(frame.sequence);
                }
            });
        }
        assert_eq!(
            buffer.upload_count(),
            1,
            "a settled page must stop adding to the upload counter"
        );
    }

    /// Two renderers drawing one source must both see the pixels that changed
    /// while they were behind. The first one to acknowledge must not make the
    /// source discard the dirt the second one still needs.
    #[test]
    fn dirty_regions_wait_for_every_consumer() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(8, 8, 0x22);
        let first = ExternalTextureConsumerId::next();
        let second = ExternalTextureConsumerId::next();
        let dirty = |x: i32, y: i32| Bounds {
            origin: point(DevicePixels(x), DevicePixels(y)),
            size: size(DevicePixels(2), DevicePixels(2)),
        };

        // Both renderers upload the first frame (a recreate is a full upload).
        assert!(buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, &[]));
        buffer.with_frame(&mut |frame| {
            buffer.mark_uploaded_for(first, frame.sequence);
            buffer.mark_uploaded_for(second, frame.sequence);
        });

        // Frame 2: the first renderer uploads it, the second has not yet.
        assert!(buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &pixels,
            &[dirty(0, 0)]
        ));
        buffer.with_frame(&mut |frame| {
            assert_eq!(frame.dirty, [dirty(0, 0)]);
            buffer.mark_uploaded_for(first, frame.sequence);
        });

        // Frame 3 arrives before the second renderer drew frame 2, so its
        // regions are the union, not just this frame's.
        assert!(buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &pixels,
            &[dirty(4, 4)]
        ));
        buffer.with_frame(&mut |frame| {
            assert_eq!(
                frame.dirty,
                [dirty(0, 0), dirty(4, 4)],
                "a renderer that has not drawn frame 2 still needs its pixels"
            );
            buffer.mark_uploaded_for(second, frame.sequence);
            buffer.mark_uploaded_for(first, frame.sequence);
        });

        // Both are caught up, so the next frame's dirt replaces the list.
        assert!(buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &pixels,
            &[dirty(6, 6)]
        ));
        buffer.with_frame(&mut |frame| {
            assert_eq!(frame.dirty, [dirty(6, 6)]);
        });
    }

    /// The visit uploads to the GPU. A producer submitting the next frame must
    /// not wait for that, or Chromium's paint callback blocks on our bandwidth.
    #[test]
    fn a_visit_does_not_block_the_producer() {
        use std::sync::mpsc;

        let buffer = Arc::new(ExternalTextureBuffer::new());
        let pixels = bgra(4, 4, 0x33);
        assert!(buffer.submit(dims(4, 4), 16, ExternalTextureFormat::Bgra8, &pixels, &[]));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let visitor_buffer = Arc::clone(&buffer);
        let visitor = std::thread::spawn(move || {
            visitor_buffer.with_frame(&mut |frame| {
                entered_tx.send(()).unwrap();
                // Stand in for a GPU upload: the producer's next submit has to
                // get through while this is in flight.
                release_rx.recv().unwrap();
                assert!(frame.is_well_formed());
            });
        });
        entered_rx.recv().unwrap();

        let (submitted_tx, submitted_rx) = mpsc::channel();
        let producer_buffer = Arc::clone(&buffer);
        let producer = std::thread::spawn(move || {
            let ok =
                producer_buffer.submit(dims(4, 4), 16, ExternalTextureFormat::Bgra8, &pixels, &[]);
            submitted_tx.send(ok).unwrap();
        });
        let submitted = submitted_rx.recv_timeout(std::time::Duration::from_secs(5));
        release_tx.send(()).unwrap();
        visitor.join().unwrap();
        producer.join().unwrap();
        assert_eq!(submitted.expect("a submit must not wait for a visit"), true);
    }

    #[test]
    fn ids_are_unique_per_source() {
        let a = ExternalTextureBuffer::new();
        let b = ExternalTextureBuffer::new();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn a_submitted_frame_is_visible_to_the_renderer() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(4, 2, 0x7f);
        assert!(buffer.submit(dims(4, 2), 16, ExternalTextureFormat::Bgra8, &pixels, &[]));
        assert!(buffer.has_frame());

        let mut observed = None;
        buffer.with_frame(&mut |frame| {
            assert!(frame.is_well_formed());
            observed = Some((frame.size, frame.sequence, frame.generation, frame.format));
            assert!(frame.bytes.iter().all(|byte| *byte == 0x7f));
        });
        assert_eq!(
            observed,
            Some((dims(4, 2), 1, 1, ExternalTextureFormat::Bgra8))
        );
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_read_out_of_bounds() {
        let buffer = ExternalTextureBuffer::new();
        let too_small = bgra(4, 1, 0);
        assert!(!buffer.submit(
            dims(4, 2),
            16,
            ExternalTextureFormat::Bgra8,
            &too_small,
            &[]
        ));
        assert!(!buffer.has_frame());
    }

    #[test]
    fn a_stride_narrower_than_the_row_is_refused() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(4, 2, 0);
        assert!(!buffer.submit(dims(4, 2), 8, ExternalTextureFormat::Bgra8, &pixels, &[]));
    }

    #[test]
    fn a_settled_page_uploads_nothing() {
        // PERF-B04 in one test: submit once, paint three times, and only the
        // first paint may upload.
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(8, 8, 1);
        buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, &[]);

        let mut cached: Option<ExternalTextureCacheKey> = None;
        let mut plans = Vec::new();
        for _ in 0..3 {
            buffer.with_frame(&mut |frame| {
                let plan = plan_external_texture_update(cached, &frame);
                plans.push(plan);
                if !matches!(plan, ExternalTextureUpdate::Reuse) {
                    buffer.mark_uploaded(frame.sequence);
                    cached = Some(ExternalTextureCacheKey {
                        id: buffer.id(),
                        generation: frame.generation,
                        sequence: frame.sequence,
                        size: frame.size,
                    });
                }
            });
        }
        assert_eq!(
            plans,
            vec![
                ExternalTextureUpdate::Recreate,
                ExternalTextureUpdate::Reuse,
                ExternalTextureUpdate::Reuse
            ]
        );
        assert_eq!(buffer.upload_count(), 1);
    }

    #[test]
    fn a_new_frame_at_the_same_size_uploads_only_the_dirty_rects() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(8, 8, 1);
        buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, &[]);
        let mut cached = None;
        buffer.with_frame(&mut |frame| {
            buffer.mark_uploaded(frame.sequence);
            cached = Some(ExternalTextureCacheKey {
                id: buffer.id(),
                generation: frame.generation,
                sequence: frame.sequence,
                size: frame.size,
            });
        });

        let dirty = [Bounds {
            origin: point(DevicePixels(2), DevicePixels(3)),
            size: dims(4, 2),
        }];
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 2),
            &dirty,
        );

        let mut plan = None;
        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| {
            plan = Some(plan_external_texture_update(cached, &frame));
            regions = frame.dirty_regions();
        });
        assert_eq!(plan, Some(ExternalTextureUpdate::UploadDirty));
        assert_eq!(regions, dirty.to_vec());
    }

    #[test]
    fn a_resize_forces_the_renderer_to_recreate_its_texture() {
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 1),
            &[],
        );
        let mut cached = None;
        buffer.with_frame(&mut |frame| {
            cached = Some(ExternalTextureCacheKey {
                id: buffer.id(),
                generation: frame.generation,
                sequence: frame.sequence,
                size: frame.size,
            });
        });

        buffer.submit(
            dims(16, 8),
            64,
            ExternalTextureFormat::Bgra8,
            &bgra(16, 8, 1),
            &[],
        );
        let mut plan = None;
        buffer.with_frame(&mut |frame| {
            plan = Some(plan_external_texture_update(cached, &frame));
            assert_eq!(frame.generation, 2, "a reallocation bumps the generation");
        });
        assert_eq!(plan, Some(ExternalTextureUpdate::Recreate));
    }

    #[test]
    fn dirt_accumulates_while_the_renderer_is_behind() {
        // Two frames land between paints. The renderer must be told about both
        // regions, or half the page keeps stale pixels.
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 0),
            &[],
        );
        buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));

        let first = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: dims(2, 2),
        };
        let second = Bounds {
            origin: point(DevicePixels(4), DevicePixels(4)),
            size: dims(2, 2),
        };
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 1),
            &[first],
        );
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 2),
            &[second],
        );

        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| regions = frame.dirty_regions());
        assert_eq!(regions, vec![first, second]);
    }

    #[test]
    fn an_empty_dirty_list_means_the_whole_frame() {
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(6, 4),
            24,
            ExternalTextureFormat::Bgra8,
            &bgra(6, 4, 0),
            &[],
        );
        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| regions = frame.dirty_regions());
        assert_eq!(
            regions,
            vec![Bounds {
                origin: point(DevicePixels(0), DevicePixels(0)),
                size: dims(6, 4),
            }]
        );
    }

    #[test]
    fn dirty_rects_are_clamped_to_the_buffer() {
        // The rect has to arrive on a frame that is *not* a reallocation,
        // because a reallocation uploads the whole texture and discards
        // per-rect bookkeeping. The interesting case is a stale rect from a
        // larger previous frame arriving after a shrink.
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 0),
            &[],
        );
        buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));

        let outside = Bounds {
            origin: point(DevicePixels(4), DevicePixels(4)),
            size: dims(64, 64),
        };
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 1),
            &[outside],
        );
        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| regions = frame.dirty_regions());
        assert_eq!(
            regions,
            vec![Bounds {
                origin: point(DevicePixels(4), DevicePixels(4)),
                size: dims(4, 4),
            }],
            "a producer's rect must never index past the texture"
        );
    }

    #[test]
    fn the_first_frame_after_a_reallocation_uploads_everything() {
        // The behaviour the test above had to work around, asserted directly:
        // a new texture has no pixels, so a partial upload would leave the rest
        // of the pane as whatever the driver had in that memory.
        let buffer = ExternalTextureBuffer::new();
        let corner = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: dims(1, 1),
        };
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 0),
            &[corner],
        );
        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| regions = frame.dirty_regions());
        assert_eq!(
            regions,
            vec![Bounds {
                origin: point(DevicePixels(0), DevicePixels(0)),
                size: dims(8, 8),
            }]
        );
    }

    #[test]
    fn acknowledging_from_inside_the_visitor_does_not_deadlock() {
        // The renderers do exactly this: they learn the sequence they uploaded
        // from the frame view, and acknowledge it before the view goes out of
        // scope. The visit must stay free of every lock a source holds, or a
        // source that takes one in `with_frame` would hang the render thread on
        // the very first painted frame.
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(4, 4),
            16,
            ExternalTextureFormat::Bgra8,
            &bgra(4, 4, 3),
            &[],
        );
        buffer.with_frame(&mut |frame| {
            buffer.mark_uploaded(frame.sequence);
        });
        assert_eq!(buffer.upload_count(), 1);
    }

    #[test]
    fn an_acknowledged_frame_stops_re_reporting_its_dirty_rects() {
        // The clean-up that used to live in `mark_uploaded` now happens on the
        // next submit, so this is where it has to be proven.
        let buffer = ExternalTextureBuffer::new();
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 0),
            &[],
        );
        buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));

        let first = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: dims(2, 2),
        };
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 1),
            &[first],
        );
        buffer.with_frame(&mut |frame| {
            assert_eq!(frame.dirty_regions(), vec![first]);
            buffer.mark_uploaded(frame.sequence);
        });

        let second = Bounds {
            origin: point(DevicePixels(4), DevicePixels(4)),
            size: dims(2, 2),
        };
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 2),
            &[second],
        );
        let mut regions = Vec::new();
        buffer.with_frame(&mut |frame| regions = frame.dirty_regions());
        assert_eq!(
            regions,
            vec![second],
            "the acknowledged rect must not be uploaded a second time"
        );
    }

    #[test]
    fn releasing_a_frame_keeps_the_identity_but_drops_the_pixels() {
        let buffer = ExternalTextureBuffer::new();
        let id = buffer.id();
        buffer.submit(
            dims(8, 8),
            32,
            ExternalTextureFormat::Bgra8,
            &bgra(8, 8, 1),
            &[],
        );
        buffer.release_frame();
        assert!(!buffer.has_frame());
        assert_eq!(buffer.id(), id);
        let mut seen = 0;
        buffer.with_frame(&mut |_| seen += 1);
        assert_eq!(seen, 0);
    }
    #[test]
    fn full_repaint_dominates_partial_frames_in_both_orders() {
        for full_first in [true, false] {
            let buffer = ExternalTextureBuffer::new();
            let pixels = bgra(8, 8, 1);
            buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, &[]);
            buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));
            let partial = [Bounds {
                origin: point(DevicePixels(1), DevicePixels(1)),
                size: dims(2, 2),
            }];
            let (first, second): (&[_], &[_]) = if full_first {
                (&[], &partial)
            } else {
                (&partial, &[])
            };
            buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, first);
            buffer.submit(
                dims(8, 8),
                32,
                ExternalTextureFormat::Bgra8,
                &pixels,
                second,
            );
            buffer.with_frame(&mut |frame| {
                assert_eq!(
                    frame.dirty_regions(),
                    vec![Bounds {
                        origin: point(DevicePixels(0), DevicePixels(0)),
                        size: dims(8, 8)
                    }]
                );
            });
        }
    }

    #[test]
    fn missed_partial_frames_have_bounded_dirty_bookkeeping() {
        let buffer = ExternalTextureBuffer::new();
        let pixels = bgra(8, 8, 1);
        buffer.submit(dims(8, 8), 32, ExternalTextureFormat::Bgra8, &pixels, &[]);
        buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));
        let partial = [Bounds {
            origin: point(DevicePixels(1), DevicePixels(1)),
            size: dims(2, 2),
        }];
        for _ in 0..1000 {
            buffer.submit(
                dims(8, 8),
                32,
                ExternalTextureFormat::Bgra8,
                &pixels,
                &partial,
            );
        }
        buffer.with_frame(&mut |frame| assert!(frame.dirty.is_empty()));
    }

    #[test]
    fn overflowing_frame_geometry_is_rejected() {
        let frame = ExternalFrameView {
            size: dims(1, 2),
            stride: usize::MAX / 2 + 1,
            format: ExternalTextureFormat::Bgra8,
            bytes: &[],
            generation: 1,
            sequence: 1,
            dirty: &[],
        };
        assert!(!frame.is_well_formed());
    }

    #[test]
    fn dirty_rectangles_cannot_overflow_when_clipped() {
        let dirty = [Bounds {
            origin: point(DevicePixels(i32::MAX - 1), DevicePixels(0)),
            size: dims(8, 8),
        }];
        let pixels = bgra(8, 8, 0);
        let frame = ExternalFrameView {
            size: dims(8, 8),
            stride: 32,
            format: ExternalTextureFormat::Bgra8,
            bytes: &pixels,
            generation: 1,
            sequence: 1,
            dirty: &dirty,
        };
        assert_eq!(
            frame.dirty_regions(),
            vec![Bounds {
                origin: point(DevicePixels(0), DevicePixels(0)),
                size: dims(8, 8),
            }]
        );
    }

    #[test]
    fn unusable_dirty_hints_cannot_acknowledge_stale_pixels() {
        let buffer = ExternalTextureBuffer::new();
        let mut texture = bgra(4, 4, 0);
        buffer.submit(dims(4, 4), 16, ExternalTextureFormat::Bgra8, &texture, &[]);
        buffer.with_frame(&mut |frame| buffer.mark_uploaded(frame.sequence));
        let next = bgra(4, 4, 17);
        let dirty = [Bounds {
            origin: point(DevicePixels(100), DevicePixels(100)),
            size: dims(1, 1),
        }];
        buffer.submit(dims(4, 4), 16, ExternalTextureFormat::Bgra8, &next, &dirty);
        buffer.with_frame(&mut |frame| {
            for region in frame.dirty_regions() {
                let left = region.origin.x.0 as usize * 4;
                let right = left + region.size.width.0 as usize * 4;
                for y in region.origin.y.0..region.origin.y.0 + region.size.height.0 {
                    let row = y as usize * frame.stride;
                    texture[row + left..row + right]
                        .copy_from_slice(&frame.bytes[row + left..row + right]);
                }
            }
            buffer.mark_uploaded(frame.sequence);
        });
        assert_eq!(
            texture, next,
            "an acknowledged frame must update the texture"
        );
    }
}
