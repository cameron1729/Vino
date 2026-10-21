// SPDX-License-Identifier: GPL-2.0-or-later

//! Borrowed logical RGB images backed by either packed RGB or an immutable native capture.
//! Native views never own or mutate storage: their caller retains the capture lease through
//! planning, encoding and GPU completion. Cursor composition is a read-only pixel operation.

use crate::kshim::{Error, EINVAL};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// Compare tightly packed native pixels exactly, ignoring each unused X/alpha byte.
/// Callers with row padding pass only the visible span of each row. No alignment
/// is required; invalid lengths compare unequal instead of reading partial pixels.
#[inline]
pub fn xrgb8888_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || !a.len().is_multiple_of(4) {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    let mut offset = 0;
    #[cfg(not(target_arch = "x86_64"))]
    let offset = 0;
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        // SAFETY: x86_64 guarantees SSE2; AVX2 code is compiled only when the
        // build enables it. Each unaligned load stays within the checked slices.
        unsafe {
            #[cfg(target_feature = "avx2")]
            {
                let mask = _mm256_set1_epi32(0x00ff_ffff);
                while a.len() - offset >= 128 {
                    let mut difference = _mm256_setzero_si256();
                    for at in [0, 32, 64, 96] {
                        let left = _mm256_loadu_si256(a.as_ptr().add(offset + at).cast());
                        let right = _mm256_loadu_si256(b.as_ptr().add(offset + at).cast());
                        difference = _mm256_or_si256(difference, _mm256_xor_si256(left, right));
                    }
                    difference = _mm256_and_si256(difference, mask);
                    if _mm256_testz_si256(difference, difference) == 0 {
                        return false;
                    }
                    offset += 128;
                }
                while a.len() - offset >= 32 {
                    let difference = _mm256_and_si256(
                        _mm256_xor_si256(
                            _mm256_loadu_si256(a.as_ptr().add(offset).cast()),
                            _mm256_loadu_si256(b.as_ptr().add(offset).cast()),
                        ),
                        mask,
                    );
                    if _mm256_testz_si256(difference, difference) == 0 {
                        return false;
                    }
                    offset += 32;
                }
            }
            let mask = _mm_set1_epi32(0x00ff_ffff);
            let zero = _mm_setzero_si128();
            #[cfg(not(target_feature = "avx2"))]
            while a.len() - offset >= 64 {
                let mut difference = zero;
                for at in [0, 16, 32, 48] {
                    let left = _mm_loadu_si128(a.as_ptr().add(offset + at).cast());
                    let right = _mm_loadu_si128(b.as_ptr().add(offset + at).cast());
                    difference = _mm_or_si128(difference, _mm_xor_si128(left, right));
                }
                difference = _mm_and_si128(difference, mask);
                if _mm_movemask_epi8(_mm_cmpeq_epi32(difference, zero)) != 0xffff {
                    return false;
                }
                offset += 64;
            }
            while a.len() - offset >= 16 {
                let difference = _mm_and_si128(
                    _mm_xor_si128(
                        _mm_loadu_si128(a.as_ptr().add(offset).cast()),
                        _mm_loadu_si128(b.as_ptr().add(offset).cast()),
                    ),
                    mask,
                );
                if _mm_movemask_epi8(_mm_cmpeq_epi32(difference, zero)) != 0xffff {
                    return false;
                }
                offset += 16;
            }
        }
    }
    let (a_words, a_tail) = a[offset..].as_chunks::<8>();
    let (b_words, b_tail) = b[offset..].as_chunks::<8>();
    for (left, right) in a_words.iter().zip(b_words) {
        if (u64::from_le_bytes(*left) ^ u64::from_le_bytes(*right)) & 0x00ff_ffff_00ff_ffff != 0 {
            return false;
        }
    }
    a_tail.is_empty() || a_tail[..3] == b_tail[..3]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NativeAllocationId {
    pub pool: u64,
    pub slot: u32,
}

#[derive(Clone, Debug)]
pub struct CursorBitmap {
    pixels: Vec<u32>,
    width: usize,
    height: usize,
    stride_words: usize,
    shape_revision: u64,
}
impl CursorBitmap {
    /// Canonical 0xAARRGGBB pixels. X-format input must set alpha to 255.
    pub fn new(
        width: usize,
        height: usize,
        stride_words: usize,
        pixels: Vec<u32>,
    ) -> Result<Self, Error> {
        if width == 0
            || height == 0
            || stride_words < width
            || stride_words
                .checked_mul(height)
                .is_none_or(|n| n > pixels.len())
        {
            return Err(EINVAL);
        }
        static NEXT_SHAPE: AtomicU64 = AtomicU64::new(1);
        Ok(Self {
            pixels,
            width,
            height,
            stride_words,
            shape_revision: NEXT_SHAPE.fetch_add(1, Ordering::Relaxed),
        })
    }
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
    pub fn words(&self) -> &[u32] {
        &self.pixels
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }
    pub fn stride_words(&self) -> usize {
        self.stride_words
    }
    pub fn shape_revision(&self) -> u64 {
        self.shape_revision
    }
}

#[derive(Clone, Debug, Default)]
pub struct CursorSnapshot {
    bitmap: Option<Arc<CursorBitmap>>,
    position: (i32, i32),
    revision: u64,
}
impl CursorSnapshot {
    pub fn new(bitmap: Option<Arc<CursorBitmap>>, position: (i32, i32), revision: u64) -> Self {
        Self {
            bitmap,
            position,
            revision,
        }
    }
    pub fn bitmap(&self) -> Option<&CursorBitmap> {
        self.bitmap.as_deref()
    }
    pub fn position(&self) -> (i32, i32) {
        self.position
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Clip using wide signed arithmetic: event coordinates may be i32::MIN/MAX.
    pub fn clipped_bounds(&self, width: usize, height: usize) -> Option<[usize; 4]> {
        let bitmap = self.bitmap()?;
        let (left, top) = (i64::from(self.position.0), i64::from(self.position.1));
        let x0 = left.max(0).min(width as i64) as usize;
        let y0 = top.max(0).min(height as i64) as usize;
        let x1 = (left + bitmap.width as i64).max(0).min(width as i64) as usize;
        let y1 = (top + bitmap.height as i64).max(0).min(height as i64) as usize;
        (x0 < x1 && y0 < y1).then_some([x0, y0, x1, y1])
    }

    #[inline]
    fn blend(&self, x: usize, y: usize, rgb: [u8; 3]) -> [u8; 3] {
        let Some(bitmap) = self.bitmap() else {
            return rgb;
        };
        let cx = x as i64 - i64::from(self.position.0);
        let cy = y as i64 - i64::from(self.position.1);
        if cx < 0 || cy < 0 || cx >= bitmap.width as i64 || cy >= bitmap.height as i64 {
            return rgb;
        }
        let pixel = bitmap.pixels[cy as usize * bitmap.stride_words + cx as usize];
        let alpha = pixel >> 24;
        let foreground = [(pixel >> 16) & 255, (pixel >> 8) & 255, pixel & 255];
        std::array::from_fn(|i| {
            let mixed = u32::from(rgb[i]) * (255 - alpha) + foreground[i] * alpha;
            ((mixed + ((mixed + 1) << 8)) >> 16) as u8
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSurface<'a> {
    pixels: &'a [u8],
    width: usize,
    height: usize,
    visible_height: usize,
    stride: usize,
    allocation: NativeAllocationId,
    cursor: Option<&'a CursorSnapshot>,
    cursor_bounds: Option<[usize; 4]>,
}
impl<'a> NativeSurface<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pixels: &'a [u8],
        width: usize,
        height: usize,
        visible_height: usize,
        stride: usize,
        allocation: NativeAllocationId,
        cursor: Option<&'a CursorSnapshot>,
    ) -> Result<Self, Error> {
        if width == 0
            || height == 0
            || visible_height == 0
            || visible_height > height
            || width > u16::MAX as usize
            || height > u16::MAX as usize
            || width.checked_mul(4).is_none_or(|n| stride < n)
            || !stride.is_multiple_of(4)
            || stride
                .checked_mul(visible_height)
                .is_none_or(|n| n > pixels.len())
        {
            return Err(EINVAL);
        }
        Ok(Self {
            pixels,
            width,
            height,
            visible_height,
            stride,
            allocation,
            cursor,
            cursor_bounds: cursor.and_then(|c| c.clipped_bounds(width, visible_height)),
        })
    }
    pub fn pixels(self) -> &'a [u8] {
        self.pixels
    }
    pub fn width(self) -> usize {
        self.width
    }
    pub fn height(self) -> usize {
        self.height
    }
    pub fn visible_height(self) -> usize {
        self.visible_height
    }
    pub fn stride(self) -> usize {
        self.stride
    }
    pub fn allocation(self) -> NativeAllocationId {
        self.allocation
    }
    pub fn cursor(self) -> Option<&'a CursorSnapshot> {
        self.cursor
    }

    #[inline]
    pub fn rgb_at(self, x: usize, y: usize) -> [u8; 3] {
        assert!(x < self.width && y < self.height);
        if y >= self.visible_height {
            return [0; 3];
        }
        let p = &self.pixels[y * self.stride + x * 4..];
        let rgb = [p[2], p[1], p[0]];
        self.cursor.map_or(rgb, |cursor| cursor.blend(x, y, rgb))
    }

    /// Eight native pixels become three canonical RGB hash words entirely in registers.
    #[inline]
    pub(crate) fn rgb_words_8(self, x: usize, y: usize) -> [u64; 3] {
        assert!(x + 8 <= self.width && y < self.height);
        if y >= self.visible_height {
            return [0; 3];
        }
        let overlaps = self
            .cursor_bounds
            .is_some_and(|[left, top, right, bottom]| {
                y >= top && y < bottom && x < right && x + 8 > left
            });
        #[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
        if !overlaps {
            use std::arch::x86_64::*;
            let input = &self.pixels[y * self.stride + x * 4..];
            // SAFETY: this build enables SSSE3 and eight validated same-row pixels
            // provide 32 readable bytes. No aligned pointer requirement is imposed.
            unsafe {
                let mask = _mm_setr_epi8(
                    2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -128, -128, -128, -128,
                );
                let first = _mm_shuffle_epi8(_mm_loadu_si128(input.as_ptr().cast()), mask);
                let second = _mm_shuffle_epi8(_mm_loadu_si128(input.as_ptr().add(16).cast()), mask);
                return [
                    _mm_cvtsi128_si64(first) as u64,
                    (_mm_cvtsi128_si64(_mm_srli_si128::<8>(first)) as u64)
                        | ((_mm_cvtsi128_si64(second) as u64) << 32),
                    _mm_cvtsi128_si64(_mm_srli_si128::<4>(second)) as u64,
                ];
            }
        }
        let mut rgb = [0u8; 24];
        if overlaps {
            for i in 0..8 {
                rgb[i * 3..][..3].copy_from_slice(&self.rgb_at(x + i, y));
            }
        } else {
            let input = &self.pixels[y * self.stride + x * 4..];
            for i in 0..8 {
                let p = &input[i * 4..];
                rgb[i * 3..][..3].copy_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        [
            u64::from_le_bytes(rgb[..8].try_into().unwrap()),
            u64::from_le_bytes(rgb[8..16].try_into().unwrap()),
            u64::from_le_bytes(rgb[16..].try_into().unwrap()),
        ]
    }

    /// Exact background comparison, ignoring unused X, stride padding and cursor state.
    pub fn background_eq(self, other: Self) -> bool {
        if (self.width, self.height, self.visible_height)
            != (other.width, other.height, other.visible_height)
        {
            return false;
        }
        for y in 0..self.visible_height {
            let left = &self.pixels[y * self.stride..][..self.width * 4];
            let right = &other.pixels[y * other.stride..][..self.width * 4];
            if !xrgb8888_eq(left, right) {
                return false;
            }
        }
        true
    }

    /// Compare a rectangle's logical RGB pixels exactly, including each snapshot's cursor.
    ///
    /// Native X bytes and stride padding are ignored; rows below each view's visible height
    /// are black even when a cursor extends into them. Invalid/out-of-bounds rectangles compare
    /// unequal. Empty rectangles inside both logical surfaces compare equal.
    ///
    /// Cursor-free spans use the native SIMD comparator directly. Only the union of the old
    /// and new cursor footprints needs scalar composition. A shared immutable backing slice
    /// can skip background reads, but allocation IDs and cursor revision numbers alone cannot
    /// establish pixel identity.
    pub fn region_eq(self, other: Self, x: usize, y: usize, width: usize, height: usize) -> bool {
        let (Some(right), Some(bottom)) = (x.checked_add(width), y.checked_add(height)) else {
            return false;
        };
        if right > self.width
            || right > other.width
            || bottom > self.height
            || bottom > other.height
        {
            return false;
        }
        if width == 0 || height == 0 {
            return true;
        }
        let same_backing =
            self.pixels.as_ptr() == other.pixels.as_ptr() && self.stride == other.stride;
        let cursor_intersects = |bounds: Option<[usize; 4]>| {
            bounds.is_some_and(|[left, top, end, lower]| {
                x < end && right > left && y < lower && bottom > top
            })
        };
        if self.visible_height == other.visible_height
            && !cursor_intersects(self.cursor_bounds)
            && !cursor_intersects(other.cursor_bounds)
        {
            // Almost every strip is outside both cursor footprints. Cursor-only updates reuse
            // the immutable capture, so those strips need neither pixel reads nor a row loop.
            if same_backing {
                return true;
            }
            for row in y..bottom.min(self.visible_height) {
                if !xrgb8888_eq(
                    &self.pixels[row * self.stride + x * 4..row * self.stride + right * 4],
                    &other.pixels[row * other.stride + x * 4..row * other.stride + right * 4],
                ) {
                    return false;
                }
            }
            return true;
        }
        let background_eq = |row: usize, start: usize, end: usize| {
            if start == end {
                return true;
            }
            match (row < self.visible_height, row < other.visible_height) {
                (false, false) => true,
                (true, true) => {
                    same_backing
                        || xrgb8888_eq(
                            &self.pixels
                                [row * self.stride + start * 4..row * self.stride + end * 4],
                            &other.pixels
                                [row * other.stride + start * 4..row * other.stride + end * 4],
                        )
                }
                (left_visible, _) => {
                    let visible = if left_visible { self } else { other };
                    visible.pixels[row * visible.stride + start * 4..row * visible.stride + end * 4]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| pixel[..3] == [0; 3])
                }
            }
        };
        for row in y..bottom {
            let span = |bounds: Option<[usize; 4]>| {
                bounds
                    .filter(|&[left, top, end, bottom]| {
                        row >= top && row < bottom && x < end && right > left
                    })
                    .map(|[left, _, end, _]| (left.max(x), end.min(right)))
                    .unwrap_or((right, right))
            };
            let mut spans = [span(self.cursor_bounds), span(other.cursor_bounds)];
            if spans[1].0 < spans[0].0 {
                spans.swap(0, 1);
            }
            let mut compared = x;
            for (start, end) in spans {
                if start > compared && !background_eq(row, compared, start) {
                    return false;
                }
                for column in start.max(compared)..end {
                    if self.rgb_at(column, row) != other.rgb_at(column, row) {
                        return false;
                    }
                }
                compared = compared.max(end);
            }
            if !background_eq(row, compared, right) {
                return false;
            }
        }
        true
    }

    /// Fill a reusable row-major strip mask with exact logical RGB changes.
    ///
    /// Within each strip-height band, native rows are read from left to right so unchanged
    /// captures retain sequential memory access. A changed strip is skipped on later rows.
    /// Strips touching either cursor use the composited rectangle comparator once instead.
    /// Both surfaces must have the same logical dimensions, divisible by the strip dimensions.
    pub fn changed_strips(
        self,
        other: Self,
        strip_width: usize,
        strip_height: usize,
        changed: &mut [bool],
    ) -> Result<usize, Error> {
        if strip_width == 0
            || strip_height == 0
            || (self.width, self.height) != (other.width, other.height)
            || !self.width.is_multiple_of(strip_width)
            || !self.height.is_multiple_of(strip_height)
            || (self.width / strip_width).checked_mul(self.height / strip_height)
                != Some(changed.len())
        {
            return Err(EINVAL);
        }
        changed.fill(false);
        let columns = self.width / strip_width;
        let mut count = 0;
        if self.visible_height != other.visible_height {
            // Rare geometry transition: one view may contain visible pixels where the other
            // supplies black padding. Keep the full logical comparator for that case.
            for (index, flag) in changed.iter_mut().enumerate() {
                *flag = !self.region_eq(
                    other,
                    (index % columns) * strip_width,
                    (index / columns) * strip_height,
                    strip_width,
                    strip_height,
                );
                count += usize::from(*flag);
            }
            return Ok(count);
        }
        let same_backing =
            self.pixels.as_ptr() == other.pixels.as_ptr() && self.stride == other.stride;
        let strip_bytes = strip_width * 4;
        for (band_index, flags) in changed.chunks_exact_mut(columns).enumerate() {
            let y = band_index * strip_height;
            let bottom = y + strip_height;
            let cursor_columns = |bounds: Option<[usize; 4]>| {
                bounds
                    .filter(|&[_, top, _, lower]| y < lower && bottom > top)
                    .map(|[left, _, right, _]| (left / strip_width, right.div_ceil(strip_width)))
                    .unwrap_or((columns, columns))
            };
            let mut cursor_spans = [
                cursor_columns(self.cursor_bounds),
                cursor_columns(other.cursor_bounds),
            ];
            if cursor_spans[1].0 < cursor_spans[0].0 {
                cursor_spans.swap(0, 1);
            }
            // At most two cursor intervals leave at most three background-only intervals.
            // Compute these once per band, not once for every native row or strip.
            let mut background_spans = [(0, 0); 3];
            let mut span_count = 0;
            let mut next_column = 0;
            let mut remaining = 0;
            for (start, end) in cursor_spans {
                if start > next_column {
                    background_spans[span_count] = (next_column, start);
                    span_count += 1;
                    remaining += start - next_column;
                }
                for (column, flag) in flags
                    .iter_mut()
                    .enumerate()
                    .take(end)
                    .skip(start.max(next_column))
                {
                    *flag =
                        !self.region_eq(other, column * strip_width, y, strip_width, strip_height);
                    count += usize::from(*flag);
                }
                next_column = next_column.max(end);
            }
            if next_column < columns {
                background_spans[span_count] = (next_column, columns);
                span_count += 1;
                remaining += columns - next_column;
            }
            if same_backing || remaining == 0 {
                continue;
            }
            for row in y..bottom.min(self.visible_height) {
                let left_row = &self.pixels[row * self.stride..][..self.width * 4];
                let right_row = &other.pixels[row * other.stride..][..other.width * 4];
                for &(start, end) in &background_spans[..span_count] {
                    let left = &left_row[start * strip_bytes..end * strip_bytes];
                    let right = &right_row[start * strip_bytes..end * strip_bytes];
                    for ((flag, left), right) in flags[start..end]
                        .iter_mut()
                        .zip(left.chunks_exact(strip_bytes))
                        .zip(right.chunks_exact(strip_bytes))
                    {
                        if !*flag && !xrgb8888_eq(left, right) {
                            *flag = true;
                            count += 1;
                            remaining -= 1;
                        }
                    }
                }
                if remaining == 0 {
                    break;
                }
            }
        }
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SurfaceView<'a> {
    Rgb {
        width: usize,
        height: usize,
        pixels: &'a [u8],
    },
    Native(NativeSurface<'a>),
}
impl<'a> SurfaceView<'a> {
    pub fn rgb(width: usize, height: usize, pixels: &'a [u8]) -> Result<Self, Error> {
        if width == 0
            || height == 0
            || width.checked_mul(height).and_then(|n| n.checked_mul(3)) != Some(pixels.len())
        {
            return Err(EINVAL);
        }
        Ok(Self::Rgb {
            width,
            height,
            pixels,
        })
    }
    pub fn width(self) -> usize {
        match self {
            Self::Rgb { width, .. } => width,
            Self::Native(s) => s.width(),
        }
    }
    pub fn height(self) -> usize {
        match self {
            Self::Rgb { height, .. } => height,
            Self::Native(s) => s.height(),
        }
    }
    pub fn is_valid(self) -> bool {
        match self {
            Self::Rgb {
                width,
                height,
                pixels,
            } => Self::rgb(width, height, pixels).is_ok(),
            Self::Native(_) => true,
        }
    }
    #[inline]
    pub fn rgb_at(self, x: usize, y: usize) -> [u8; 3] {
        match self {
            Self::Rgb {
                width,
                height,
                pixels,
            } => {
                assert!(x < width && y < height);
                pixels[(y * width + x) * 3..][..3].try_into().unwrap()
            }
            Self::Native(s) => s.rgb_at(x, y),
        }
    }
    #[inline]
    pub(crate) fn rgb_byte(self, y: usize, byte: usize) -> u8 {
        match self {
            Self::Rgb { width, pixels, .. } => pixels[y * width * 3 + byte],
            Self::Native(s) => s.rgb_at(byte / 3, y)[byte % 3],
        }
    }
    #[inline]
    pub(crate) fn rgb_word(self, y: usize, byte: usize) -> u64 {
        match self {
            Self::Rgb { width, pixels, .. } => {
                u64::from_le_bytes(pixels[y * width * 3 + byte..][..8].try_into().unwrap())
            }
            Self::Native(s) => {
                if y >= s.visible_height {
                    return 0;
                }
                let x = byte / 3;
                let shift = byte % 3;
                let count = if shift == 2 { 4 } else { 3 };
                if s.cursor_bounds.is_some_and(|[left, top, right, bottom]| {
                    y >= top && y < bottom && x < right && x + count > left
                }) {
                    let mut packed = 0u128;
                    for i in 0..count {
                        let rgb = s.rgb_at(x + i, y);
                        packed |=
                            u128::from(u32::from_le_bytes([rgb[0], rgb[1], rgb[2], 0])) << (24 * i);
                    }
                    return (packed >> (shift * 8)) as u64;
                }
                let input = &s.pixels[y * s.stride + x * 4..];
                #[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
                if x + 4 <= s.width {
                    // Native release builds use one register shuffle for the common
                    // cursor-free case. The fourth pixel must be inside the same row;
                    // the final three-pixel word takes the bounded scalar path below.
                    use std::arch::x86_64::*;
                    // SAFETY: SSSE3 is enabled for this build; four validated native
                    // pixels give 16 readable bytes, and unaligned loads are allowed.
                    unsafe {
                        let pixels = _mm_loadu_si128(input.as_ptr().cast());
                        let mask = _mm_setr_epi8(
                            2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -128, -128, -128, -128,
                        );
                        let packed = _mm_shuffle_epi8(pixels, mask);
                        let shifted = match shift {
                            0 => packed,
                            1 => _mm_srli_si128::<1>(packed),
                            _ => _mm_srli_si128::<2>(packed),
                        };
                        return _mm_cvtsi128_si64(shifted) as u64;
                    }
                }
                let mut packed = 0u128;
                for i in 0..count {
                    let p = &input[i * 4..];
                    packed |= u128::from(u32::from_le_bytes([p[2], p[1], p[0], 0])) << (24 * i);
                }
                (packed >> (shift * 8)) as u64
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native<'a>(
        pixels: &'a [u8],
        width: usize,
        height: usize,
        visible: usize,
        stride: usize,
        cursor: Option<&'a CursorSnapshot>,
    ) -> NativeSurface<'a> {
        NativeSurface::new(
            pixels,
            width,
            height,
            visible,
            stride,
            NativeAllocationId { pool: 1, slot: 0 },
            cursor,
        )
        .unwrap()
    }

    /// Independent scalar oracle: decode BGR bytes, clip the cursor with signed coordinates
    /// and divide the weighted RGB sum by 255. It does not use rgb_at/blend or SIMD helpers.
    fn scalar_rgb(surface: NativeSurface<'_>, x: usize, y: usize) -> [u8; 3] {
        if y >= surface.visible_height {
            return [0; 3];
        }
        let offset = y * surface.stride + x * 4;
        let background = [
            surface.pixels[offset + 2],
            surface.pixels[offset + 1],
            surface.pixels[offset],
        ];
        let Some(cursor) = surface.cursor else {
            return background;
        };
        let Some(bitmap) = cursor.bitmap.as_ref() else {
            return background;
        };
        let cx = x as i64 - i64::from(cursor.position.0);
        let cy = y as i64 - i64::from(cursor.position.1);
        if !(0..bitmap.width as i64).contains(&cx) || !(0..bitmap.height as i64).contains(&cy) {
            return background;
        }
        let pixel = bitmap.pixels[cy as usize * bitmap.stride_words + cx as usize];
        let alpha = pixel >> 24;
        std::array::from_fn(|channel| {
            let foreground = (pixel >> (16 - channel * 8)) & 255;
            ((u32::from(background[channel]) * (255 - alpha) + foreground * alpha) / 255) as u8
        })
    }

    fn scalar_region_eq(
        left: NativeSurface<'_>,
        right: NativeSurface<'_>,
        region: [usize; 4],
    ) -> bool {
        let [x, y, width, height] = region;
        (y..y + height).all(|row| {
            (x..x + width).all(|col| scalar_rgb(left, col, row) == scalar_rgb(right, col, row))
        })
    }

    fn assert_strip_mask(
        left: NativeSurface<'_>,
        right: NativeSurface<'_>,
        strip_width: usize,
        strip_height: usize,
    ) {
        let columns = left.width / strip_width;
        let expected: Vec<bool> = (0..(left.height / strip_height) * columns)
            .map(|index| {
                !scalar_region_eq(
                    left,
                    right,
                    [
                        (index % columns) * strip_width,
                        (index / columns) * strip_height,
                        strip_width,
                        strip_height,
                    ],
                )
            })
            .collect();
        let mut flags = vec![true; expected.len()];
        let count = left
            .changed_strips(right, strip_width, strip_height, &mut flags)
            .unwrap();
        assert_eq!(flags, expected);
        assert_eq!(count, expected.iter().filter(|&&flag| flag).count());
        // Reuse the same mask for unchanged pixels: no previous change may remain latched.
        assert_eq!(
            left.changed_strips(left, strip_width, strip_height, &mut flags)
                .unwrap(),
            0
        );
        assert!(flags.iter().all(|&flag| !flag));
    }

    fn patterned_native(width: usize, visible: usize, stride: usize) -> Vec<u8> {
        let mut bytes = vec![0xa5; visible * stride];
        for row in 0..visible {
            for col in 0..width {
                let at = row * stride + col * 4;
                bytes[at..at + 3].copy_from_slice(&[
                    (row * 17 + col * 3) as u8,
                    (row * 11 + col * 7) as u8,
                    (row * 5 + col * 19) as u8,
                ]);
            }
        }
        bytes
    }

    #[test]
    fn native_regions_compare_rgb_exactly_across_stride_simd_and_last_row_boundaries() {
        let width = 193;
        let stride_left = width * 4 + 12;
        let stride_right = width * 4 + 28;
        let bytes = patterned_native(width, 3, stride_left);
        let left = native(&bytes, width, 5, 3, stride_left, None);
        let mut different = patterned_native(width, 3, stride_right);
        for row in 0..3 {
            for col in 0..width {
                different[row * stride_right + col * 4 + 3] = 0x22;
            }
            different[row * stride_right + width * 4..(row + 1) * stride_right].fill(0x77);
        }
        let mut regions = vec![[0, 0, width, 5], [0, 3, width, 2], [64, 2, 64, 3]];
        for x in [0, 1, 3, 7, 15, 31, 63, 64, 127, 192] {
            for span in [1, 2, 3, 4, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
                if x + span <= width {
                    regions.push([x, 1, span, 3]);
                }
            }
        }
        let right = native(&different, width, 5, 3, stride_right, None);
        for &[x, y, w, h] in &regions {
            assert!(left.region_eq(right, x, y, w, h));
        }
        // Both constructors intentionally give every view the same allocation ID. That is not
        // permission to skip reads of a different backing slice or another generation's pixels.
        for row in 0..3 {
            for col in 0..width {
                for channel in 0..3 {
                    let at = row * stride_right + col * 4 + channel;
                    different[at] ^= 1;
                    let right = native(&different, width, 5, 3, stride_right, None);
                    assert!(!left.region_eq(right, 0, 0, width, 5));
                    assert!(!left.region_eq(right, col, row, 1, 1));
                    for region in [[0, 0, 64, 3], [64, 1, 64, 3], [128, 2, 65, 3]] {
                        let [x, y, w, h] = region;
                        assert_eq!(
                            left.region_eq(right, x, y, w, h),
                            scalar_region_eq(left, right, region),
                            "changed pixel ({col},{row}), channel={channel}, region={region:?}"
                        );
                    }
                    different[at] ^= 1;
                }
            }
        }
    }

    #[test]
    fn native_region_geometry_and_padding_are_checked_before_reading_storage() {
        let bytes = patterned_native(8, 2, 40);
        let left = native(&bytes, 8, 4, 2, 40, None);
        for [x, y, w, h] in [
            [usize::MAX, 0, 1, 1],
            [0, usize::MAX, 1, 1],
            [1, 0, usize::MAX, 1],
            [0, 1, 1, usize::MAX],
            [8, 0, 1, 1],
            [0, 4, 1, 1],
            [9, 0, 0, 0],
        ] {
            assert!(!left.region_eq(left, x, y, w, h));
        }
        assert!(left.region_eq(left, 8, 4, 0, 0));
        assert!(left.region_eq(left, 3, 1, 0, 2));
        assert!(left.region_eq(left, 2, 4, 3, 0));
        assert!(left.region_eq(left, 0, 2, 8, 2));
        // Same starting pointer with a different row stride does not identify equal pixels.
        let shifted = native(&bytes, 8, 4, 2, 36, None);
        assert!(left.region_eq(shifted, 0, 0, 8, 1));
        assert!(!left.region_eq(shifted, 0, 1, 8, 1));
        assert_strip_mask(left, shifted, 4, 2);
        let narrower = native(&bytes, 7, 4, 2, 40, None);
        assert!(!left.region_eq(narrower, 0, 0, 8, 1));
        assert!(left.region_eq(narrower, 0, 0, 7, 1));

        let mut more_rows = bytes.clone();
        more_rows.extend_from_slice(&[0, 0, 0, 255].repeat(20));
        let right = native(&more_rows, 8, 4, 4, 40, None);
        assert!(left.region_eq(right, 0, 0, 8, 4));
        assert!(right.region_eq(left, 0, 0, 8, 4));
        let cursor = CursorSnapshot::new(
            Some(Arc::new(
                CursorBitmap::new(1, 2, 1, vec![0xff123456; 2]).unwrap(),
            )),
            (7, 2),
            1,
        );
        let padded_cursor = native(&bytes, 8, 4, 2, 40, Some(&cursor));
        let visible_cursor = native(&more_rows, 8, 4, 4, 40, Some(&cursor));
        assert!(padded_cursor.region_eq(right, 0, 0, 8, 4));
        assert!(!padded_cursor.region_eq(visible_cursor, 0, 0, 8, 4));
        assert!(!visible_cursor.region_eq(padded_cursor, 0, 0, 8, 4));
        assert_strip_mask(left, right, 4, 2);
        assert_strip_mask(padded_cursor, visible_cursor, 4, 2);
        assert_strip_mask(visible_cursor, padded_cursor, 4, 2);
    }

    #[test]
    fn native_regions_match_scalar_oracle_for_cursor_moves_shapes_clipping_and_overlap() {
        let (width, visible, height, stride) = (129, 17, 20, 536);
        let bytes = patterned_native(width, visible, stride);
        let mut bitmap_words = vec![0xffff00ff; 10 * 4];
        for row in 0..4 {
            for col in 0..7 {
                let alpha = [0u32, 1, 127, 128, 254, 255][(row + col) % 6];
                bitmap_words[row * 10 + col] =
                    alpha << 24 | ((row * 19 + col * 13) as u32 * 0x010307) & 0x00ff_ffff;
            }
        }
        let bitmap = Arc::new(CursorBitmap::new(7, 4, 10, bitmap_words.clone()).unwrap());
        bitmap_words[10 + 3] ^= 0x00ff_ff00;
        let reshaped = Arc::new(CursorBitmap::new(7, 4, 10, bitmap_words).unwrap());
        let transparent = Arc::new(CursorBitmap::new(7, 4, 7, vec![0x00123456; 28]).unwrap());
        let mut cursors = vec![CursorSnapshot::new(None, (0, 0), 9)];
        // All revisions deliberately match: revision numbers are not pixel identities.
        for position in [
            (-3, -2),
            (0, 0),
            (60, 13),
            (63, 14),
            (64, 14),
            (126, 16),
            (50, 5),
            (110, 5),
            (i32::MIN, 0),
            (0, i32::MAX),
        ] {
            cursors.push(CursorSnapshot::new(Some(bitmap.clone()), position, 9));
        }
        cursors.push(CursorSnapshot::new(Some(reshaped), (63, 14), 9));
        cursors.push(CursorSnapshot::new(Some(transparent), (63, 14), 9));
        let regions = [
            [0, 0, width, height],
            [0, 0, 64, 16],
            [64, 0, 64, 16],
            [128, 0, 1, 16],
            [0, 16, 64, 4],
            [64, 16, 64, 4],
            [128, 16, 1, 4],
            [62, 13, 8, 4],
            [50, 5, 67, 4],
            [0, 17, width, 3],
        ];
        for left_cursor in std::iter::once(None).chain(cursors.iter().map(Some)) {
            for right_cursor in std::iter::once(None).chain(cursors.iter().map(Some)) {
                let left = native(&bytes, width, height, visible, stride, left_cursor);
                let right = native(&bytes, width, height, visible, stride, right_cursor);
                for region in regions {
                    let [x, y, w, h] = region;
                    assert_eq!(
                        left.region_eq(right, x, y, w, h),
                        scalar_region_eq(left, right, region),
                        "cursor {:?} vs {:?}, region={region:?}",
                        left_cursor.map(CursorSnapshot::position),
                        right_cursor.map(CursorSnapshot::position)
                    );
                }
                assert_strip_mask(left, right, 43, 4);
            }
        }
    }

    #[test]
    fn native_regions_compare_composited_pixels_when_cursor_masks_background_changes() {
        let original = [20, 30, 40, 99].repeat(8 * 3);
        let mut changed = original.clone();
        changed[8 * 4 + 3 * 4..][..3].copy_from_slice(&[21, 31, 41]);
        for alpha in 0..=255u32 {
            let cursor = CursorSnapshot::new(
                Some(Arc::new(
                    CursorBitmap::new(1, 1, 1, vec![(alpha << 24) | 0x00c8c8c8]).unwrap(),
                )),
                (3, 1),
                1,
            );
            let left = native(&original, 8, 3, 3, 32, Some(&cursor));
            let right = native(&changed, 8, 3, 3, 32, Some(&cursor));
            assert_eq!(
                left.region_eq(right, 0, 0, 8, 3),
                scalar_region_eq(left, right, [0, 0, 8, 3]),
                "alpha={alpha}"
            );
            assert_strip_mask(left, right, 4, 1);
            if alpha == 0 {
                assert!(!left.region_eq(right, 0, 0, 8, 3));
            } else if alpha >= 254 {
                assert!(left.region_eq(right, 0, 0, 8, 3));
            }
            // Hiding the cursor reveals the exact background change again.
            assert!(!native(&original, 8, 3, 3, 32, None).region_eq(
                native(&changed, 8, 3, 3, 32, None),
                0,
                0,
                8,
                3
            ));
        }
    }

    #[test]
    fn native_strip_batch_marks_sparse_edges_and_dense_changes_without_reading_padding() {
        let (width, height, visible) = (128, 48, 47);
        let left_bytes = patterned_native(width, visible, width * 4 + 12);
        let left = native(&left_bytes, width, height, visible, width * 4 + 12, None);
        let mut right_bytes = patterned_native(width, visible, width * 4 + 28);
        for row in 0..visible {
            for col in 0..width {
                right_bytes[row * (width * 4 + 28) + col * 4 + 3] ^= 0xff;
            }
        }
        assert_strip_mask(
            left,
            native(&right_bytes, width, height, visible, width * 4 + 28, None),
            64,
            16,
        );
        // First-row changes exercise early completion; bottom-right changes must wait for the
        // last visible row. Two adjacent strips can finish on very different native rows.
        for (x, y) in [(0, 0), (127, 15), (63, 31), (64, 16), (0, 32), (127, 46)] {
            let at = y * (width * 4 + 28) + x * 4;
            right_bytes[at] ^= 1;
            let right = native(&right_bytes, width, height, visible, width * 4 + 28, None);
            assert_strip_mask(left, right, 64, 16);
            assert_strip_mask(left, right, 8, 4);
            assert_strip_mask(left, right, 128, 48);
        }
        for row in 0..visible {
            for col in 0..width {
                right_bytes[row * (width * 4 + 28) + col * 4 + 1] ^= 1;
            }
        }
        let right = native(&right_bytes, width, height, visible, width * 4 + 28, None);
        assert_strip_mask(left, right, 64, 16);
        assert_strip_mask(left, right, 1, 1);
    }

    #[test]
    fn native_strip_batch_rejects_invalid_geometry_without_changing_output_mask() {
        let bytes = patterned_native(128, 32, 512);
        let left = native(&bytes, 128, 32, 32, 512, None);
        for (width, height, length) in [
            (0, 16, 4),
            (64, 0, 4),
            (63, 16, 4),
            (64, 15, 4),
            (64, 16, 3),
        ] {
            let mut flags = vec![true; length];
            assert!(left
                .changed_strips(left, width, height, &mut flags)
                .is_err());
            assert!(flags.iter().all(|&flag| flag));
        }
        let right = native(&bytes, 64, 32, 32, 512, None);
        let mut flags = vec![true; 4];
        assert!(left.changed_strips(right, 64, 16, &mut flags).is_err());
        assert!(flags.iter().all(|&flag| flag));
    }

    #[test]
    fn native_equality_masks_x_and_checks_all_color_bytes_at_unaligned_tails() {
        for pixels in [
            0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 65, 193, 1920,
        ] {
            for alignment in [0, 1, 3, 7, 15, 31] {
                let length = pixels * 4;
                let a: Vec<u8> = (0..length + alignment + 3)
                    .map(|i| (i * 17) as u8)
                    .collect();
                let mut b = a.clone();
                let left = &a[alignment..alignment + length];
                let right = &mut b[alignment..alignment + length];
                for pixel in right.as_chunks_mut::<4>().0 {
                    pixel[3] ^= 0xff;
                }
                assert!(
                    xrgb8888_eq(left, right),
                    "X changed; pixels={pixels} alignment={alignment}"
                );
                // Every channel at every position must survive SIMD chunk boundaries
                // and final scalar tails, not just the first differing block.
                for i in 0..length {
                    if i % 4 == 3 {
                        continue;
                    }
                    right[i] ^= 1;
                    assert!(
                        !xrgb8888_eq(left, right),
                        "missed byte {i}; pixels={pixels} alignment={alignment}"
                    );
                    right[i] ^= 1;
                }
                assert!(xrgb8888_eq(left, right));
            }
        }
        assert!(!xrgb8888_eq(&[0; 3], &[0; 3]));
        assert!(!xrgb8888_eq(&[0; 4], &[0; 8]));
    }

    #[test]
    fn native_words_match_rgb_and_ignore_unused_storage() {
        for width in [1, 3, 8, 13, 64, 128] {
            let stride = width * 4 + 16;
            let mut bytes = vec![0xa5; stride * 3];
            let mut rgb = vec![0; width * 4 * 3];
            for y in 0..3 {
                for x in 0..width {
                    let color = [
                        (x * 19 + y) as u8,
                        (x + y * 73) as u8,
                        (x * 7 + y * 9) as u8,
                    ];
                    bytes[y * stride + x * 4..][..3]
                        .copy_from_slice(&[color[2], color[1], color[0]]);
                    rgb[(y * width + x) * 3..][..3].copy_from_slice(&color);
                }
            }
            let s = native(&bytes, width, 4, 3, stride, None);
            let view = SurfaceView::Native(s);
            for y in 0..4 {
                for x in 0..width {
                    assert_eq!(view.rgb_at(x, y), rgb[(y * width + x) * 3..][..3]);
                }
                for byte in 0..width * 3 {
                    assert_eq!(view.rgb_byte(y, byte), rgb[y * width * 3 + byte]);
                }
                for byte in 0..(width * 3).saturating_sub(7) {
                    assert_eq!(
                        view.rgb_word(y, byte),
                        u64::from_le_bytes(rgb[y * width * 3 + byte..][..8].try_into().unwrap())
                    );
                }
            }
            let mut different = bytes.clone();
            for y in 0..3 {
                for x in 0..width {
                    different[y * stride + x * 4 + 3] ^= 0xff;
                }
                different[y * stride + width * 4..(y + 1) * stride].fill(77);
            }
            assert!(s.background_eq(native(&different, width, 4, 3, stride, None)));
            different[2] ^= 1;
            assert!(!s.background_eq(native(&different, width, 4, 3, stride, None)));
        }
    }

    #[test]
    fn rejects_bad_native_geometry_without_reading_pixels() {
        let bytes = [0; 32];
        let id = NativeAllocationId { pool: 0, slot: 0 };
        for (w, h, v, stride) in [
            (0, 2, 2, 16),
            (4, 1, 2, 16),
            (4, 2, 2, 15),
            (4, 2, 2, 20),
            (4, 2, 0, 16),
        ] {
            assert!(NativeSurface::new(&bytes, w, h, v, stride, id, None).is_err());
        }
    }

    #[test]
    fn immutable_cursor_snapshot_composes_without_touching_capture_or_padding() {
        let bytes = [100, 60, 20, 55].repeat(4);
        let bitmap = Arc::new(CursorBitmap::new(1, 1, 1, vec![0x805028c8]).unwrap());
        let first = CursorSnapshot::new(Some(bitmap.clone()), (0, 0), 1);
        let moved = CursorSnapshot::new(Some(bitmap), (1, 0), 2);
        let original = native(&bytes, 2, 3, 2, 8, None);
        let a = native(&bytes, 2, 3, 2, 8, Some(&first));
        let b = native(&bytes, 2, 3, 2, 8, Some(&moved));
        assert_eq!(a.rgb_at(0, 0), [50, 49, 150]);
        assert_eq!(b.rgb_at(0, 0), [20, 60, 100]);
        assert_eq!(b.rgb_at(1, 0), [50, 49, 150]);
        assert_eq!(a.rgb_at(0, 2), [0; 3]);
        assert!(a.background_eq(b));
        assert_eq!(original.rgb_at(0, 0), [20, 60, 100]);
    }
}
