// SPDX-License-Identifier: GPL-2.0-or-later

//! Exact per-strip content identities for immutable native captures.
//!
//! The caller retains both consumed input leases during `update`, then releases the old lease
//! before encoding. Revisions describe the latest consumed logical image, not the last successful
//! USB presentation; the scanout planner separately owns committed identities and retransmit debt.

use crate::{
    kshim::{Error, EINVAL},
    kvino::DockProfile,
    surface::NativeSurface,
};
use std::sync::Arc;

pub struct NativeChanges {
    width: usize,
    height: usize,
    strip_width: usize,
    strip_height: usize,
    // Pointer identity is unique while either tracker or planner retains the allocation. Unlike
    // a wrapping numeric tracker ID, a new tracker/reset can never alias a live planner domain.
    domain: Arc<()>,
    generation: u64,
    revisions: Vec<u64>,
    changed: Vec<bool>,
    changed_strips: usize,
}

impl NativeChanges {
    pub fn new(dock: DockProfile) -> Self {
        let (strip_width, strip_height) = dock.strip_dims();
        Self {
            width: 0,
            height: 0,
            strip_width,
            strip_height,
            domain: Arc::new(()),
            generation: 0,
            revisions: Vec::new(),
            changed: Vec::new(),
            changed_strips: 0,
        }
    }

    /// Compare against the exact logical input supplied on the preceding update, including its
    /// immutable cursor snapshot. `None` starts a new comparison domain and requires a keyframe.
    /// Advance the caller's baseline even when this returns false: hidden background or cursor
    /// metadata can change without changing logical RGB. No EVDI damage hint is trusted here.
    pub fn update(
        &mut self,
        previous: Option<NativeSurface<'_>>,
        current: NativeSurface<'_>,
    ) -> Result<bool, Error> {
        self.update_using(
            previous,
            current,
            true,
            |previous, current, width, height, flags| {
                current.changed_strips(previous, width, height, flags)?;
                Ok(())
            },
        )
    }

    /// Use an exact external comparator for new capture allocations. The callback must fill
    /// every flag in row-major strip order, comparing logical RGB including both cursors.
    /// First frames, resets and same-backing cursor updates retain the CPU path. Revisions
    /// only advance after successful comparison, so a failed GPU operation is retryable.
    pub fn update_with<E: From<Error>>(
        &mut self,
        previous: Option<NativeSurface<'_>>,
        current: NativeSurface<'_>,
        compare: impl FnOnce(
            NativeSurface<'_>,
            NativeSurface<'_>,
            usize,
            usize,
            &mut [bool],
        ) -> Result<(), E>,
    ) -> Result<bool, E> {
        self.update_using(previous, current, false, compare)
    }

    fn update_using<E: From<Error>>(
        &mut self,
        previous: Option<NativeSurface<'_>>,
        current: NativeSurface<'_>,
        duplicate_probe: bool,
        compare: impl FnOnce(
            NativeSurface<'_>,
            NativeSurface<'_>,
            usize,
            usize,
            &mut [bool],
        ) -> Result<(), E>,
    ) -> Result<bool, E> {
        let (width, height) = (current.width(), current.height());
        if !width.is_multiple_of(self.strip_width) || !height.is_multiple_of(self.strip_height) {
            return Err(EINVAL.into());
        }
        let reset = self.width != width
            || self.height != height
            || previous.is_none_or(|p| p.width() != width || p.height() != height)
            || self.generation == u64::MAX;
        if reset {
            self.domain = Arc::new(());
            self.width = width;
            self.height = height;
            self.generation = 1;
            let count = (width / self.strip_width) * (height / self.strip_height);
            self.revisions.resize(count, 1);
            self.revisions.fill(1);
            self.changed.resize(count, true);
            self.changed_strips = count;
            return Ok(true);
        }
        let previous = previous.expect("reset handles a missing baseline");
        // Once input has settled, a sequential equality probe is cheaper than maintaining
        // thousands of strip flags for another duplicate capture. A first difference falls
        // through to the exact strip scan; continuously changing input never pays this probe.
        // Same-backing cursor-only updates already skip background reads in changed_strips.
        if duplicate_probe
            && self.changed_strips == 0
            && previous.pixels().as_ptr() != current.pixels().as_ptr()
            && current.region_eq(previous, 0, 0, width, height)
        {
            return Ok(false);
        }
        let next_generation = self.generation + 1;
        self.changed.fill(false);
        if previous.pixels().as_ptr() == current.pixels().as_ptr()
            && previous.stride() == current.stride()
        {
            current.changed_strips(
                previous,
                self.strip_width,
                self.strip_height,
                &mut self.changed,
            )?;
        } else {
            compare(
                previous,
                current,
                self.strip_width,
                self.strip_height,
                &mut self.changed,
            )?;
        }
        self.changed_strips = self.changed.iter().filter(|&&changed| changed).count();
        for (revision, &changed) in self.revisions.iter_mut().zip(&self.changed) {
            if changed {
                *revision = next_generation;
            }
        }
        if self.changed_strips != 0 {
            self.generation = next_generation;
        }
        Ok(self.changed_strips != 0)
    }

    /// Strips whose logical pixels changed since the previous consumed input.
    /// This differs from changes since the last successful USB presentation after a failed send.
    pub fn changed_strips(&self) -> usize {
        self.changed_strips
    }

    pub(crate) fn domain(&self) -> &Arc<()> {
        &self.domain
    }

    pub(crate) fn revisions(&self) -> &[u64] {
        &self.revisions
    }

    pub(crate) fn matches(&self, surface: NativeSurface<'_>, dock: DockProfile) -> bool {
        self.generation != 0
            && (self.width, self.height) == (surface.width(), surface.height())
            && (self.strip_width, self.strip_height) == dock.strip_dims()
            && self.revisions.len()
                == (self.width / self.strip_width) * (self.height / self.strip_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::NativeAllocationId;

    fn view(bytes: &[u8], width: usize, height: usize, slot: u32) -> NativeSurface<'_> {
        NativeSurface::new(
            bytes,
            width,
            height,
            height,
            width * 4,
            NativeAllocationId { pool: 1, slot },
            None,
        )
        .unwrap()
    }

    #[test]
    fn exact_revisions_change_only_affected_strips_and_ignore_x() {
        let dock = DockProfile::for_family(crate::kvino::Family::Ella).unwrap();
        let mut changes = NativeChanges::new(dock);
        let a = vec![0; 128 * 32 * 4];
        let mut b = a.clone();
        assert!(changes.update(None, view(&a, 128, 32, 0)).unwrap());
        assert_eq!(changes.revisions, [1, 1, 1, 1]);
        for byte in b.iter_mut().skip(3).step_by(4) {
            *byte = 0xff;
        }
        assert!(!changes
            .update(Some(view(&a, 128, 32, 0)), view(&b, 128, 32, 1))
            .unwrap());
        assert_eq!(changes.revisions, [1, 1, 1, 1]);
        let duplicate = b.clone();
        assert!(!changes
            .update(Some(view(&b, 128, 32, 1)), view(&duplicate, 128, 32, 2))
            .unwrap());
        assert_eq!(changes.revisions, [1, 1, 1, 1]);
        assert_eq!(changes.changed_strips(), 0);
        let mut c = b.clone();
        c[(31 * 128 + 127) * 4] = 1;
        assert!(changes
            .update(Some(view(&duplicate, 128, 32, 2)), view(&c, 128, 32, 3))
            .unwrap());
        assert_eq!(changes.revisions, [1, 1, 1, 2]);
        assert_eq!(changes.changed_strips(), 1);
        let mut d = c.clone();
        d[0] = 2;
        d[64 * 4 + 2] = 3;
        changes
            .update(Some(view(&c, 128, 32, 2)), view(&d, 128, 32, 3))
            .unwrap();
        assert_eq!(changes.revisions, [3, 3, 1, 2]);
    }

    #[test]
    fn external_comparison_failure_does_not_commit_partial_flags() {
        let dock = DockProfile::for_family(crate::kvino::Family::Ella).unwrap();
        let mut changes = NativeChanges::new(dock);
        let a = vec![0; 128 * 32 * 4];
        let mut b = a.clone();
        b[0] = 1;
        changes.update(None, view(&a, 128, 32, 0)).unwrap();
        let domain = Arc::clone(changes.domain());
        let failed: Result<bool, Error> = changes.update_with(
            Some(view(&a, 128, 32, 0)),
            view(&b, 128, 32, 1),
            |_, _, _, _, flags| {
                flags.fill(true);
                Err(EINVAL)
            },
        );
        assert!(failed.is_err());
        assert!(Arc::ptr_eq(&domain, changes.domain()));
        assert_eq!(changes.revisions, [1; 4]);
        assert_eq!(changes.generation, 1);
        assert_eq!(changes.changed_strips(), 4);
        let retried: Result<bool, Error> = changes.update_with(
            Some(view(&a, 128, 32, 0)),
            view(&b, 128, 32, 1),
            |previous, current, width, height, flags| {
                assert!(flags.iter().all(|&flag| !flag));
                current.changed_strips(previous, width, height, flags)?;
                Ok(())
            },
        );
        assert!(retried.unwrap());
        assert_eq!(changes.revisions, [2, 1, 1, 1]);
        assert_eq!(changes.changed_strips(), 1);
    }

    #[test]
    fn external_comparison_skips_first_frame_and_same_backing_cursor_path() {
        let dock = DockProfile::for_family(crate::kvino::Family::Ella).unwrap();
        let mut changes = NativeChanges::new(dock);
        let a = vec![0; 128 * 32 * 4];
        let surface = view(&a, 128, 32, 0);
        let no_call = |_: NativeSurface<'_>,
                       _: NativeSurface<'_>,
                       _: usize,
                       _: usize,
                       _: &mut [bool]|
         -> Result<(), Error> {
            panic!("GPU comparison is unnecessary for reset or same-backing input")
        };
        assert!(changes.update_with(None, surface, no_call).unwrap());
        assert!(!changes
            .update_with(Some(surface), surface, no_call)
            .unwrap());
    }

    #[test]
    fn rollover_resize_and_missing_baseline_create_new_domains() {
        let dock = DockProfile::for_family(crate::kvino::Family::Ella).unwrap();
        let mut changes = NativeChanges::new(dock);
        let pixels = vec![0; 128 * 32 * 4];
        let image = view(&pixels, 128, 32, 0);
        changes.update(None, image).unwrap();
        let old_domain = Arc::clone(changes.domain());
        changes.generation = u64::MAX;
        assert!(changes.update(Some(image), image).unwrap());
        assert!(!Arc::ptr_eq(&old_domain, changes.domain()));
        assert_eq!(changes.revisions, [1; 4]);
        let old_domain = Arc::clone(changes.domain());
        let resized = view(&pixels, 64, 64, 0);
        changes.update(Some(image), resized).unwrap();
        assert!(!Arc::ptr_eq(&old_domain, changes.domain()));
        assert_eq!(changes.revisions, [1; 4]);
        let old_domain = Arc::clone(changes.domain());
        changes.update(None, resized).unwrap();
        assert!(!Arc::ptr_eq(&old_domain, changes.domain()));
    }
}
