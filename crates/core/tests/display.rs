// SPDX-License-Identifier: GPL-3.0-or-later

//! A card picture's own reduction: `focusforge_core::display::Downsampler`,
//! fed in bands that need not divide the target size evenly.

mod display {
    mod downsample {
        use focusforge_core::display::{Downsampler, center_crop};
        use focusforge_core::tile::Rect;

        /// A deterministic source: sample `(x, y)`'s one channel is
        /// `y * width + x`, so a reduced cell's expected mean is computable
        /// by hand.
        fn source(width: u32, height: u32) -> Vec<u16> {
            (0..height)
                .flat_map(|y| (0..width).map(move |x| (y * width + x) as u16))
                .collect()
        }

        #[test]
        fn center_crop_matches_target_aspect_and_stays_inside_the_source() {
            // Wider than the target: the crop narrows, height untouched.
            let crop = center_crop(400, 100, 16, 9);
            assert_eq!(crop.height, 100);
            assert!(crop.width <= 400);
            assert_eq!(crop.x, (400 - crop.width) / 2);

            // Taller than the target: the crop shortens, width untouched.
            let crop = center_crop(100, 400, 16, 9);
            assert_eq!(crop.width, 100);
            assert!(crop.height <= 400);
            assert_eq!(crop.y, (400 - crop.height) / 2);

            // Already the target aspect: nothing is cropped away.
            let crop = center_crop(32, 18, 16, 9);
            assert_eq!(
                crop,
                Rect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 18
                }
            );
        }

        #[test]
        fn full_source_reducer_keeps_landscape_and_portrait_edges() {
            let mut landscape = Downsampler::new_full_source(9, 3, 1, 3, 3);
            assert_eq!(
                landscape.crop(),
                Rect {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 3
                },
                "source previews must not inherit project-card centre cropping"
            );
            let mut portrait = Downsampler::new_full_source(3, 9, 1, 3, 3);
            assert_eq!(
                portrait.crop(),
                Rect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 9
                },
                "every original edge contributes before aspect-fit display"
            );

            landscape.add_band(
                &source(9, 3),
                Rect {
                    x: 0,
                    y: 0,
                    width: 9,
                    height: 3,
                },
            );
            portrait.add_band(
                &source(3, 9),
                Rect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 9,
                },
            );
            assert_ne!(landscape.finish(), vec![0; 9]);
            assert_ne!(portrait.finish(), vec![0; 9]);
        }

        #[test]
        fn band_accumulation_matches_a_whole_image_reduction() {
            let (width, height) = (12u32, 8u32);
            let (target_width, target_height) = (4u32, 4u32);
            let pixels = source(width, height);

            // Fed as one whole-image band.
            let mut whole = Downsampler::new(width, height, 1, target_width, target_height);
            whole.add_band(
                &pixels,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            );
            let whole_reduction = whole.finish();

            // Fed in 3-row bands, which do not divide the crop's 8 rows
            // evenly.
            let mut banded = Downsampler::new(width, height, 1, target_width, target_height);
            const BAND_ROWS: u32 = 3;
            let mut y = 0;
            while y < height {
                let rows = BAND_ROWS.min(height - y);
                let start = (y * width) as usize;
                let end = ((y + rows) * width) as usize;
                banded.add_band(
                    &pixels[start..end],
                    Rect {
                        x: 0,
                        y,
                        width,
                        height: rows,
                    },
                );
                y += rows;
            }
            let banded_reduction = banded.finish();

            assert_eq!(
                whole_reduction, banded_reduction,
                "a band-fed reduction must not depend on how the source was split"
            );
        }

        #[test]
        fn a_cell_no_band_ever_covers_reads_zero_not_a_divide_by_zero() {
            let downsampler = Downsampler::new(8, 8, 1, 4, 4);
            // `finish` is called with no `add_band` at all.
            let reduction = downsampler.finish();
            assert_eq!(reduction, vec![0u16; 16]);
        }

        #[test]
        fn a_partially_covered_band_still_folds_only_its_own_rows() {
            let (width, height) = (8u32, 8u32);
            let pixels = source(width, height);
            let mut downsampler = Downsampler::new(width, height, 1, 4, 4);
            // Two bands, one that stops short of the crop's far edge.
            downsampler.add_band(
                &pixels[0..(5 * width) as usize],
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 5,
                },
            );
            downsampler.add_band(
                &pixels[(5 * width) as usize..],
                Rect {
                    x: 0,
                    y: 5,
                    width,
                    height: 3,
                },
            );
            let split = downsampler.finish();

            let mut whole = Downsampler::new(width, height, 1, 4, 4);
            whole.add_band(
                &pixels,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            );
            assert_eq!(split, whole.finish());
        }
    }
}
