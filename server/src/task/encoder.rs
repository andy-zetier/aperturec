use crate::backend::FramebufferUpdate;
use crate::metrics::CompressionRatio;

use anyhow::{anyhow, Result};
use aperturec_channel::{AsyncReceiver, AsyncSender};
use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages as cm;
use aperturec_protocol::media_messages as mm;
use aperturec_state_machine::{
    Recovered, SelfTransitionable, State, Stateful, Transitionable, TryTransitionable,
};
use aperturec_trace::log;
use aperturec_trace::queue::{self, deq_group, enq_group, trace_queue_group};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use euclid::{Point2D, Size2D, UnknownUnit};
use flate2::{Compress, Compression, FlushCompress};
use futures::{future::join, stream, StreamExt};
use linear_map::set::LinearSet;
use ndarray::{arr0, s, Array2, ArrayView2, AssignElem, Axis, ShapeBuilder, Zip};
use parking_lot::{Mutex, MutexGuard};
use std::cmp::{min, Ordering};
use std::collections::{btree_map::Entry, BTreeMap, LinkedList};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tokio_util::sync::CancellationToken;

const DISPATCH_QUEUE: queue::QueueGroup = trace_queue_group!("encoder:dispatch");

pub const RAW_BYTES_PER_PIXEL: usize = 4;
const ENC_BYTES_PER_PIXEL: usize = 3;
const MAX_BYTES_PER_MESSAGE: usize = 1400;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawPixel {
    pub blue: u8,
    pub green: u8,
    pub red: u8,
    pub alpha: u8,
}

impl Copy for RawPixel {}

impl AssignElem<RawPixel> for &mut &mut [u8] {
    fn assign_elem(self, input: RawPixel) {
        self[0] = input.blue;
        self[1] = input.green;
        self[2] = input.red;
    }
}

pub type Point = Point2D<usize, UnknownUnit>;
pub type Size = Size2D<usize, UnknownUnit>;
pub type Rect = euclid::Rect<usize, UnknownUnit>;
pub type Box2D = euclid::Box2D<usize, UnknownUnit>;

const X_AXIS: Axis = Axis(0);
const Y_AXIS: Axis = Axis(1);

fn encode_raw(
    max_bytes_per_msg: usize,
    curr_loc: &mut Point,
    raw_data: ArrayView2<RawPixel>,
) -> (Bytes, Size) {
    const MAX_ROW_WIDTH_PIXELS: usize = 32;

    if curr_loc.x >= raw_data.len_of(X_AXIS) || curr_loc.y >= raw_data.len_of(Y_AXIS) {
        return (Bytes::new(), Size::new(0, 0));
    }

    let num_remaining_cols = raw_data.len_of(X_AXIS) - curr_loc.x;
    let width = min(MAX_ROW_WIDTH_PIXELS, num_remaining_cols);

    let num_remaining_rows = raw_data.len_of(Y_AXIS) - curr_loc.y;
    let height = min(
        max_bytes_per_msg / (ENC_BYTES_PER_PIXEL * MAX_ROW_WIDTH_PIXELS),
        num_remaining_rows,
    );

    let mut enc = BytesMut::zeroed(height * width * ENC_BYTES_PER_PIXEL);
    let slice = raw_data.slice(s![
        curr_loc.x..curr_loc.x + width,
        curr_loc.y..curr_loc.y + height
    ]);
    {
        let mut chunks_array = Array2::from_shape_vec(
            (slice.len_of(X_AXIS), slice.len_of(Y_AXIS)).f(),
            enc.chunks_mut(ENC_BYTES_PER_PIXEL).collect(),
        )
        .expect("Create chunks array");
        slice.assign_to(&mut chunks_array);
    }

    curr_loc.x += width;
    if curr_loc.x >= raw_data.len_of(X_AXIS) {
        curr_loc.x = 0;
        curr_loc.y += height;
    }

    (enc.freeze(), Size::new(width, height))
}

fn encode_zlib(
    max_bytes_per_msg: usize,
    curr_loc: &mut Point,
    raw_data: ArrayView2<RawPixel>,
) -> (Bytes, Size) {
    const MAX_ROW_WIDTH_PIXELS: usize = 32;
    const MAX_COL_HEIGHT_PIXELS: usize = 512;

    if curr_loc.x >= raw_data.len_of(X_AXIS) || curr_loc.y >= raw_data.len_of(Y_AXIS) {
        return (Bytes::new(), Size::new(0, 0));
    }

    let num_remaining_cols = raw_data.len_of(X_AXIS) - curr_loc.x;
    let num_remaining_rows = raw_data.len_of(Y_AXIS) - curr_loc.y;

    let width = min(MAX_ROW_WIDTH_PIXELS, num_remaining_cols);
    let height = min(MAX_COL_HEIGHT_PIXELS, num_remaining_rows);

    let raw_data_slice = raw_data
        .slice(s![
            curr_loc.x..curr_loc.x + width,
            curr_loc.y..curr_loc.y + height
        ])
        .reversed_axes(); // nd-array has x/y reversed

    let mut enc_data = BytesMut::zeroed(max_bytes_per_msg);
    let mut compressor = Compress::new_with_window_bits(Compression::new(1), false, 9);
    let mut nbytes_consumed = 0;
    let mut nbytes_produced = 0;
    let mut row_idx = 0;

    let finish_reserve = width * ENC_BYTES_PER_PIXEL * 2;
    let mut raw_row = vec![0_u8; width * ENC_BYTES_PER_PIXEL];
    while row_idx < height && nbytes_produced < (enc_data.len() - finish_reserve) {
        let row = raw_data_slice.row(row_idx);

        for (i, &pixel) in row.iter().enumerate() {
            let start = i * ENC_BYTES_PER_PIXEL;
            raw_row[start] = pixel.blue;
            raw_row[start + 1] = pixel.green;
            raw_row[start + 2] = pixel.red;
        }

        let output = &mut enc_data[nbytes_produced..max_bytes_per_msg - finish_reserve];
        compressor
            .compress(&raw_row, output, FlushCompress::Partial)
            .expect("zlib compress partial");
        nbytes_consumed = compressor.total_in() as usize;
        nbytes_produced = compressor.total_out() as usize;
        row_idx += 1;
    }

    let complete_rows_consumed = nbytes_consumed / (width * ENC_BYTES_PER_PIXEL);
    compressor
        .compress(&[], &mut enc_data[nbytes_produced..], FlushCompress::Finish)
        .expect("zlib compress finish");

    let ratio = 100_f64 * (nbytes_produced as f64 / nbytes_consumed as f64);
    CompressionRatio::observe(ratio);

    curr_loc.y += complete_rows_consumed;
    if curr_loc.y >= raw_data.len_of(Y_AXIS) {
        curr_loc.y = 0;
        curr_loc.x += width;
    }

    enc_data.truncate(compressor.total_out() as usize);
    (enc_data.freeze(), Size::new(width, complete_rows_consumed))
}

fn encode(
    codec: Codec,
    raw_data: ArrayView2<RawPixel>,
    max_bytes_per_msg: usize,
) -> impl Iterator<Item = (Bytes, Point, Size, Codec)> + '_ {
    let mut curr_loc = Point::new(0, 0);

    std::iter::repeat_with(move || {
        let encoded_loc = curr_loc;
        let (bytes, encoded_dim) = match codec {
            Codec::Raw => encode_raw(max_bytes_per_msg, &mut curr_loc, raw_data),
            Codec::Zlib => encode_zlib(max_bytes_per_msg, &mut curr_loc, raw_data),
            unsupported_codec => panic!("Unsupported codec {:?}", unsupported_codec),
        };
        (bytes, encoded_loc, encoded_dim)
    })
    .take_while(|(bytes, ..)| !bytes.is_empty())
    .map(move |(bytes, encoded_loc, encoded_dim)| (bytes, encoded_loc, encoded_dim, codec))
}

#[derive(Stateful, Debug)]
#[state(S)]
pub struct Encoder<S: State> {
    state: S,
}

#[derive(State, Debug)]
pub struct Created<AsyncDuplex>
where
    AsyncDuplex: AsyncSender<Message = mm::ServerToClientMessage>
        + AsyncReceiver<Message = mm::ClientToServerMessage>,
{
    send_recv: AsyncDuplex,
    codec: Codec,
    decoder_area: cm::DecoderArea,
    fb_rx: mpsc::UnboundedReceiver<Arc<FramebufferUpdate>>,
    missed_frame_rx: mpsc::UnboundedReceiver<SequenceId>,
    acked_frame_rx: mpsc::Receiver<SequenceId>,
    ct: CancellationToken,
}

impl<AsyncDuplex> SelfTransitionable for Encoder<Created<AsyncDuplex>> where
    AsyncDuplex: AsyncSender<Message = mm::ServerToClientMessage>
        + AsyncReceiver<Message = mm::ClientToServerMessage>
{
}

#[derive(State, Debug)]
pub struct Running {
    task: JoinHandle<anyhow::Result<()>>,
    ct: CancellationToken,
}

#[derive(State, Debug)]
pub struct Terminated {}

#[derive(Debug)]
pub struct Channels {
    pub acked_frame_tx: mpsc::Sender<SequenceId>,
    pub fb_tx: mpsc::UnboundedSender<Arc<FramebufferUpdate>>,
    pub missed_frame_tx: mpsc::UnboundedSender<SequenceId>,
}

impl<AsyncDuplex> Encoder<Created<AsyncDuplex>>
where
    AsyncDuplex: AsyncSender<Message = mm::ServerToClientMessage>
        + AsyncReceiver<Message = mm::ClientToServerMessage>,
{
    pub fn new(
        decoder_area: cm::DecoderArea,
        send_recv: AsyncDuplex,
        codec: Codec,
    ) -> (Self, Channels, CancellationToken) {
        let (fb_tx, fb_rx) = mpsc::unbounded_channel();
        let (missed_frame_tx, missed_frame_rx) = mpsc::unbounded_channel();
        let (acked_frame_tx, acked_frame_rx) = mpsc::channel(1);
        let ct = CancellationToken::new();

        let enc = Encoder {
            state: Created {
                send_recv,
                codec,
                decoder_area,
                fb_rx,
                missed_frame_rx,
                acked_frame_rx,
                ct: ct.clone(),
            },
        };
        (
            enc,
            Channels {
                fb_tx,
                missed_frame_tx,
                acked_frame_tx,
            },
            ct,
        )
    }
}

mod box2d {
    use super::*;

    #[derive(Debug, Clone, Default)]
    pub struct Set {
        inner: Vec<Box2D>,
    }

    impl Set {
        pub fn with_initial_box(b: Box2D) -> Self {
            Set { inner: vec![b] }
        }

        /// Add a new box to the box set
        ///
        /// The [`Set`] guarantees that all boxes within the set are non-overlapping. We can
        /// guarantee this inductively:
        ///
        /// Base case A: an empty set contains only non-overlapping boxes
        /// Base case B: a set with a single box contains only non-overlapping boxes
        /// Inductive step: a set of N boxes contains only non-overlapping boxes if the set of N-1
        /// boxes contains only N boxes, and the step to add the Nth box does not add any
        /// overlapping boxes
        ///
        /// Therefore, we must guarantee that the [`add`] function does not add any overlapping
        /// functions. When adding a new box `new` to the set, we have three simple cases and one
        /// complex case:
        ///
        /// - Simple case 1: `new` does not intersect any existing boxes -> add `new` to the set
        /// without modification
        /// - Simple case 2: `new` is completely contained within one of the existing boxes -> do
        /// not add `new` to the set
        /// - Simple case 3: `new` completely contains all existing boxes -> remove all existing
        /// boxes and just retain `new` in the set
        /// - Complex case: `new` intersects some number of existing boxes, but does not contain
        /// all of them -> use the following algorithm:
        ///
        /// Define a set of boxes S which contains only `new`
        /// For each existing box E in the set of existing boxes:
        ///     For each box B in the set S:
        ///         Intersect B with E and define the following 9 regions:
        ///          ┌─────┬─────┬─────┐
        ///          │     │     │     │
        ///          │  1  │  2  │  3  │
        ///          │     │     │     │
        ///          ├─────┼─────┼─────┤
        ///          │     │     │     │
        ///          │  4  │  I  │  5  │
        ///          │     │     │     │
        ///          ├─────┼─────┼─────┤
        ///          │     │     │     │
        ///          │  6  │  7  │  8  │
        ///          │     │     │     │
        ///          └─────┴─────┴─────┘
        ///          I is the intersection of B and E, while 1 - 9 are the potential
        ///          regions where B exists and E does not.
        ///          Add sections 1 - 9 to S if that section exists in B.
        /// Return the union of existing boxes and S
        pub fn add(&mut self, new: Box2D) {
            if self.inner.is_empty() {
                self.inner.push(new);
                return;
            }

            if self
                .inner
                .iter()
                .any(|existing| existing.contains_box(&new))
            {
                return;
            }

            if self.inner.iter().all(|existing| new.contains_box(existing)) {
                self.inner.clear();
                self.inner.push(new);
                return;
            }

            let mut uncommitted = LinkedList::new();
            uncommitted.push_back(new);
            let mut batch = [None; 8];
            for e in &self.inner {
                while let Some(b) = uncommitted.pop_front() {
                    if let Some(i) = b.intersection(e) {
                        batch[0] = Some(Box2D::new(
                            Point::new(b.min.x, b.min.y),
                            Point::new(i.min.x, i.min.y),
                        )); // 1
                        batch[1] = Some(Box2D::new(
                            Point::new(i.min.x, b.min.y),
                            Point::new(i.max.x, i.min.y),
                        )); // 2
                        batch[2] = Some(Box2D::new(
                            Point::new(i.max.x, b.min.y),
                            Point::new(b.max.x, i.min.y),
                        )); // 3
                        batch[3] = Some(Box2D::new(
                            Point::new(b.min.x, i.min.y),
                            Point::new(i.min.x, i.max.y),
                        )); // 4
                        batch[4] = Some(Box2D::new(
                            Point::new(i.max.x, i.min.y),
                            Point::new(b.max.x, i.max.y),
                        )); // 5
                        batch[5] = Some(Box2D::new(
                            Point::new(b.min.x, i.max.y),
                            Point::new(i.min.x, b.max.y),
                        )); // 6
                        batch[6] = Some(Box2D::new(
                            Point::new(i.min.x, i.max.y),
                            Point::new(i.max.x, b.max.y),
                        )); // 7
                        batch[7] = Some(Box2D::new(
                            Point::new(i.max.x, i.max.y),
                            Point::new(b.max.x, b.max.y),
                        )); // 8
                    } else {
                        batch[0] = Some(b);
                        for slot in &mut batch[1..] {
                            *slot = None;
                        }
                    }
                }
                uncommitted.extend(
                    batch
                        .iter()
                        .filter_map(|&item| item)
                        .filter(|region| !region.is_negative() && !region.is_empty()),
                );
            }

            self.inner.extend(uncommitted);
        }

        pub fn clear(&mut self) {
            self.inner.clear();
        }

        pub fn union<'i, I>(&self, other: I) -> Self
        where
            I: IntoIterator<Item = &'i Box2D>,
        {
            let mut new = self.clone();
            other.into_iter().for_each(|b| new.add(*b));
            new
        }

        pub fn iter(&self) -> Iter {
            Iter { set: self, idx: 0 }
        }

        pub fn pop(&mut self) -> Option<Box2D> {
            self.inner.pop()
        }
    }

    pub struct Iter<'b> {
        set: &'b Set,
        idx: usize,
    }

    impl<'b> IntoIterator for &'b Set {
        type Item = &'b Box2D;
        type IntoIter = Iter<'b>;

        fn into_iter(self) -> Self::IntoIter {
            self.iter()
        }
    }

    impl<'b> Iterator for Iter<'b> {
        type Item = &'b Box2D;

        fn next(&mut self) -> Option<Self::Item> {
            let output = self.set.inner.get(self.idx);
            self.idx += 1;
            output
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;

        #[test]
        fn single_box() {
            let b1 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let mut set = Set::default();
            set.add(b1);
            let boxes: Vec<_> = set.iter().collect();
            assert_eq!(boxes, vec![&b1]);
        }

        #[test]
        fn non_overlapping_boxes() {
            let b1 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let b2 = Box2D::new(Point::new(0, 15), Point::new(20, 20));
            let mut set = Set::default();
            set.add(b1);
            set.add(b2);
            let boxes: Vec<_> = set.iter().collect();
            assert_eq!(boxes, vec![&b1, &b2]);
        }

        #[test]
        fn two_identical_boxes() {
            let b1 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let b2 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let mut set = Set::default();
            set.add(b1);
            set.add(b2);
            let boxes: Vec<_> = set.iter().collect();
            assert_eq!(boxes, vec![&b1]);
        }

        #[test]
        fn subsuming_boxes() {
            let b1 = Box2D::new(Point::new(5, 5), Point::new(10, 10));
            let b2 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let mut set = Set::default();
            set.add(b1);
            set.add(b2);
            let boxes: Vec<_> = set.iter().collect();
            assert_eq!(boxes, vec![&b2]);
        }

        #[test]
        fn three_overlapping_boxes() {
            let b1 = Box2D::new(Point::new(0, 0), Point::new(10, 10));
            let b2 = Box2D::new(Point::new(5, 5), Point::new(15, 15));
            let b3 = Box2D::new(Point::new(10, 10), Point::new(20, 20));
            let mut set = Set::default();
            set.add(b1);
            set.add(b2);
            set.add(b3);
            let mut boxes: Vec<Box2D> = set.iter().map(|b| *b).collect();
            boxes.sort_by_key(|b| b.min.x);
            boxes.sort_by_key(|b| b.min.y);
            boxes.sort_by_key(|b| b.max.x);
            boxes.sort_by_key(|b| b.max.y);

            let mut other = vec![
                Box2D::new(Point::new(0, 0), Point::new(10, 10)),
                Box2D::new(Point::new(10, 5), Point::new(15, 10)),
                Box2D::new(Point::new(5, 10), Point::new(10, 15)),
                Box2D::new(Point::new(10, 10), Point::new(15, 15)),
                Box2D::new(Point::new(15, 10), Point::new(20, 15)),
                Box2D::new(Point::new(10, 15), Point::new(15, 20)),
                Box2D::new(Point::new(15, 15), Point::new(20, 20)),
            ];
            other.sort_by_key(|b| b.min.x);
            other.sort_by_key(|b| b.min.y);
            other.sort_by_key(|b| b.max.x);
            other.sort_by_key(|b| b.max.y);

            assert_eq!(boxes, other);
        }
    }
}

pub struct TrackingBuffer {
    indices: Array2<(usize, usize)>,
    data: Array2<RawPixel>,
    damaged_areas: box2d::Set,
    is_fully_damaged: bool,
    stale_areas: box2d::Set,
}

/// A `TrackingBuffer` is used to manage a 2D array of pixels, efficiently tracking changes and damaged areas.
///
/// The buffer consists of:
/// - A 2D array of pixel data (`data`).
/// - An array of indices corresponding to the pixel data (`indices`).
/// - A set of boxes representing damaged areas (`damaged_areas`).
/// - A boolean indicating if the entire buffer is damaged (`is_fully_damaged`).
/// - A set of boxes representing areas marked as stale (`stale_areas`).
///
/// This structure is particularly useful in graphical applications where partial updates to the pixel data are common.
impl TrackingBuffer {
    /// Constructs a new `TrackingBuffer`.
    ///
    /// # Arguments
    ///
    /// * `size` - A `Size` struct specifying the width and height of the buffer.
    ///
    /// # Returns
    ///
    /// Returns a new instance of `TrackingBuffer`.
    ///
    /// # Examples
    ///
    /// ```
    /// use aperturec_server::task::encoder::{Size, TrackingBuffer};
    /// let buffer = TrackingBuffer::new(Size::new(800, 600));
    /// ```
    pub fn new(size: Size) -> Self {
        TrackingBuffer {
            indices: Array2::from_shape_fn((size.width, size.height), |idx| idx),
            data: Array2::from_elem((size.width, size.height), RawPixel::default()),
            damaged_areas: box2d::Set::default(),
            is_fully_damaged: false,
            stale_areas: box2d::Set::default(),
        }
    }

    /// Updates the buffer with new pixel data at a specified origin point.
    ///
    /// # Arguments
    ///
    /// * `origin` - A `Point` struct specifying the origin point for the update.
    /// * `new` - An `ArrayView2` of `RawPixel` representing the new pixel data to be updated.
    ///
    /// # Behavior
    ///
    /// If the origin is outside the bounds of the current data, the function returns early.
    /// Otherwise, it calculates the area to be updated, checks and records the changes, and
    /// updates the damaged areas accordingly.
    ///
    /// This method is crucial for efficient redrawing and updating of the graphical interface,
    /// as it allows for tracking only the changed portions of the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use aperturec_server::task::encoder::{Point, RawPixel, Size, TrackingBuffer};
    /// use ndarray::Array2;
    /// let new_pixels = Array2::<RawPixel>::default((100, 100));
    /// let mut buffer = TrackingBuffer::new(Size::new(800, 600));
    /// buffer.update(Point::new(10, 10), new_pixels.view());
    /// ```
    pub fn update(&mut self, origin: Point, new: ArrayView2<RawPixel>) {
        // Early exit if the origin point is outside the bounds of the current data array.
        if origin.x >= self.data.len_of(X_AXIS) || origin.y >= self.data.len_of(Y_AXIS) {
            return;
        }

        // Create a bounding box (`update_box`) representing the area affected by the update.
        let update_box = Box2D::new(
            origin,
            origin + Point::new(new.len_of(X_AXIS), new.len_of(Y_AXIS)).to_vector(),
        );

        // Define the slice of the buffer that will be updated.
        let shape = s![
            update_box.min.x..update_box.max.x,
            update_box.min.y..update_box.max.y,
        ];
        let mut curr = self.data.slice_mut(shape);

        // If the entire buffer is already marked as damaged, simply assign new pixel data.
        if self.is_fully_damaged {
            curr.assign(&new);
        } else {
            // Otherwise, process the update selectively.
            // Get the corresponding encoder relative indices.
            let encoder_relative_indices = self.indices.slice(shape);

            // Initialize variables to track the extents of changed areas.
            let (mut left, mut right, mut top, mut bottom) = (None, None, None, None);

            // Iterate over each pixel in the current slice and the new data.
            Zip::from(&mut curr)
                .and(&new)
                .and(&encoder_relative_indices)
                .for_each(|curr, new, (x, y)| {
                    // If the current pixel is different from the new pixel, it's a change.
                    if curr != new {
                        if left.is_none() || *x < left.unwrap() {
                            left = Some(*x);
                        }
                        if right.is_none() || x + 1 > right.unwrap() {
                            right = Some(*x + 1);
                        }
                        if top.is_none() || *y < top.unwrap() {
                            top = Some(*y);
                        }
                        if bottom.is_none() || y + 1 > bottom.unwrap() {
                            bottom = Some(*y + 1);
                        }

                        // Update the current pixel to the new value.
                        *curr = *new;
                    }
                });

            // If no changes were made, return early.
            if (None, None, None, None) == (left, right, top, bottom) {
                return;
            }

            // Unwrap the bounds of the changed area.
            let (left, right, top, bottom) =
                (left.unwrap(), right.unwrap(), top.unwrap(), bottom.unwrap());

            // Check if the entire buffer is damaged.
            if left == 0
                && right == self.data.len_of(X_AXIS)
                && top == 0
                && bottom == self.data.len_of(Y_AXIS)
            {
                // If so, clear damaged areas and mark the buffer as fully damaged.
                self.damaged_areas.clear();
                self.is_fully_damaged = true;
            } else {
                // Otherwise, add the damaged area to the set of damaged areas.
                let min_box = Box2D::new(Point::new(left, top), Point::new(right, bottom));
                self.damaged_areas.add(min_box);
            }
        }
    }

    /// Marks a specific area of the buffer as stale.
    ///
    /// # Arguments
    ///
    /// * `stale` - A `Box2D` representing the area to be marked as stale.
    ///
    /// # Behavior
    ///
    /// This method adds the specified area to the set of stale areas in the buffer. Stale areas
    /// are those that are not currently damaged but may not be updated on the client. We track
    /// these separately from damage so that when we do the flip we do not copy unchanged pixels.
    ///
    /// # Examples
    ///
    /// ```
    /// use aperturec_server::task::encoder::{Box2D, Point, Size, TrackingBuffer};
    /// let mut buffer = TrackingBuffer::new(Size::new(800, 600));
    /// buffer.mark_stale(Box2D::new(Point::new(50, 50), Point::new(100, 100)));
    /// ```
    pub fn mark_stale(&mut self, stale: Box2D) {
        self.stale_areas.add(stale);
    }

    /// Clears all tracked damaged and stale areas in the buffer.
    ///
    /// # Behavior
    ///
    /// This method resets the state of the buffer, clearing all damaged and stale areas. It is
    /// useful when the entire buffer has been processed or redrawn and a fresh start is needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use aperturec_server::task::encoder::{Size, TrackingBuffer};
    /// let mut buffer = TrackingBuffer::new(Size::new(800, 600));
    /// buffer.clear();
    /// ```
    pub fn clear(&mut self) {
        self.damaged_areas.clear();
        self.is_fully_damaged = false;
        self.stale_areas.clear();
    }
}

struct CachedFramebuffer {
    origin: Point,
    size: Size,
    dirty_tx: watch::Sender<()>,
    front: TrackingBuffer,
    back: TrackingBuffer,
}

impl CachedFramebuffer {
    fn new(decoder_area: &cm::DecoderArea) -> Self {
        let origin = Point::new(
            decoder_area.location.x_position as usize,
            decoder_area.location.y_position as usize,
        );
        let size = Size::new(
            decoder_area.dimension.width as usize,
            decoder_area.dimension.height as usize,
        );
        let front = TrackingBuffer::new(size);
        let mut back = TrackingBuffer::new(size);
        back.is_fully_damaged = true;
        let (dirty_tx, _) = watch::channel(());
        CachedFramebuffer {
            origin,
            size,
            dirty_tx,
            front,
            back,
        }
    }

    fn dirty_watch(&self) -> watch::Receiver<()> {
        self.dirty_tx.subscribe()
    }

    fn rect(&self) -> Rect {
        Rect {
            origin: self.origin,
            size: self.size,
        }
    }

    fn update(&mut self, fb_update: Arc<FramebufferUpdate>) {
        if let Some((screen_relative_loc, new)) = fb_update.get_intersecting_data(&self.rect()) {
            let encoder_relative_loc = (screen_relative_loc - self.origin).to_point();

            self.back.update(encoder_relative_loc, new);
            self.dirty_tx.send_replace(());
        }
    }

    fn flip(&mut self) {
        self.front.damaged_areas.clear();
        self.front.stale_areas = self.back.stale_areas.clone();

        if self.back.is_fully_damaged {
            self.front.data.assign(&self.back.data);
            self.front.is_fully_damaged = true;
        } else {
            for damaged_area in &self.back.damaged_areas {
                let shape = s![
                    damaged_area.min.x..damaged_area.max.x,
                    damaged_area.min.y..damaged_area.max.y
                ];
                let mut front_data = self.front.data.slice_mut(shape);
                let back_data = self.back.data.slice(shape);
                front_data.assign(&back_data);
                self.front.damaged_areas.add(*damaged_area);
            }
            self.front.is_fully_damaged = false;
        }
        self.back.clear();
    }

    fn mark_stale(&mut self, stale: &Box2D) {
        assert!(
            Box2D {
                min: Point::new(0, 0),
                max: Point::new(self.size.width, self.size.height),
            }
            .contains_box(stale),
            "Marking {:?} as stale buffer of size {:?} (out-of-bounds)",
            stale,
            self.size
        );
        self.back.mark_stale(*stale);
        self.dirty_tx.send_replace(());
    }

    fn requires_updates(&self) -> impl Iterator<Item = (Point, ArrayView2<RawPixel>)> {
        let mut set = if self.front.is_fully_damaged {
            box2d::Set::with_initial_box(Rect::new(Point::new(0, 0), self.size).to_box2d())
        } else {
            self.front.stale_areas.union(&self.front.damaged_areas)
        };
        std::iter::from_fn(move || {
            set.pop().map(|b| {
                (
                    b.min,
                    self.front
                        .data
                        .slice(s![b.min.x..b.max.x, b.min.y..b.max.y]),
                )
            })
        })
    }
}

#[async_trait]
impl<AsyncDuplex> TryTransitionable<Running, Created<AsyncDuplex>> for Encoder<Created<AsyncDuplex>>
where
    AsyncDuplex: AsyncSender<Message = mm::ServerToClientMessage>
        + AsyncReceiver<Message = mm::ClientToServerMessage>,
{
    type SuccessStateful = Encoder<Running>;
    type FailureStateful = Encoder<Created<AsyncDuplex>>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let in_flight = Arc::new(Mutex::new(BTreeMap::<u64, (Point, Size)>::new()));
        let responsible_sequence_nos = Arc::new(Mutex::new(Array2::<u64>::zeros((
            self.state.decoder_area.dimension.width as usize,
            self.state.decoder_area.dimension.height as usize,
        ))));
        let decoder_area = self.state.decoder_area.clone();
        let cached_fb = Arc::new(Mutex::new(CachedFramebuffer::new(&decoder_area)));
        let (dispatch_tx, dispatch_rx): (mpsc::Sender<Vec<mm::RectangleUpdate>>, _) =
            mpsc::channel(3);
        let (synthetic_missed_frame_tx, synthetic_missed_frame_rx) =
            mpsc::unbounded_channel::<SequenceId>();

        let mut ack_subtask: JoinHandle<anyhow::Result<()>> = {
            let mut acked_frame_stream = ReceiverStream::new(self.state.acked_frame_rx).fuse();
            let missed_frame_stream =
                UnboundedReceiverStream::new(self.state.missed_frame_rx).fuse();
            let synthetic_missed_frame_stream =
                UnboundedReceiverStream::new(synthetic_missed_frame_rx).fuse();
            let mut mfr_stream = stream::select(missed_frame_stream, synthetic_missed_frame_stream);
            let cached_fb = cached_fb.clone();
            let in_flight = in_flight.clone();
            let responsible_sequence_nos = responsible_sequence_nos.clone();
            let ct = self.state.ct.clone();
            let mut max_acked_so_far = 0;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = ct.cancelled() => {
                            log::trace!("Ack subtask cancelled");
                            break Ok(())
                        }
                        Some(acked_frame_seq) = acked_frame_stream.next() => {
                            match acked_frame_seq.0.cmp(&max_acked_so_far) {
                                Ordering::Greater => {
                                    let mut in_flight_locked = in_flight.lock();
                                    in_flight_locked.remove(&acked_frame_seq.0);
                                    MutexGuard::unlock_fair(in_flight_locked);
                                    max_acked_so_far = acked_frame_seq.0;
                                },
                                Ordering::Less => {
                                    log::warn!("Received ack for {} before {}", max_acked_so_far, acked_frame_seq.0);
                                }
                                Ordering::Equal => ()
                            }
                        }
                        Some(missed_frame) = mfr_stream.next() => {
                            let in_flight_locked = in_flight.lock();
                            if let Some((loc, dim)) = in_flight_locked.get(&missed_frame.0) {
                                let responsible_sequence_nos_locked = responsible_sequence_nos.lock();
                                if responsible_sequence_nos_locked
                                    .slice(s![loc.x..loc.x + dim.width, loc.y..loc.y+dim.height])
                                    .iter()
                                    .any(|responsible| responsible == &missed_frame.0)
                                {
                                    let mut cached_fb_locked = cached_fb.lock();
                                    cached_fb_locked.mark_stale(&Rect::new(*loc, *dim).to_box2d());
                                    MutexGuard::unlock_fair(cached_fb_locked);
                                }
                                MutexGuard::unlock_fair(responsible_sequence_nos_locked);
                            }
                            MutexGuard::unlock_fair(in_flight_locked);
                        }
                        else => break Err(anyhow!("Ack subtask futures exhausted"))
                    }
                }
            })
        };

        let mut damage_subtask: JoinHandle<anyhow::Result<()>> = {
            let mut fb_stream = UnboundedReceiverStream::new(self.state.fb_rx).fuse();
            let cached_fb = cached_fb.clone();
            let ct = self.state.ct.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = ct.cancelled() => {
                            log::debug!("Damage subtask cancelled");
                            break Ok(());
                        }
                        Some(fb_update) = fb_stream.next() => {
                            let mut cached_fb_locked = cached_fb.lock();
                            cached_fb_locked.update(fb_update);
                            MutexGuard::unlock_fair(cached_fb_locked);
                        }
                        else => break Err(anyhow!("Damage subtask futures exhausted"))
                    }
                }
            })
        };

        let mut dispatch_subtask: JoinHandle<anyhow::Result<()>> = {
            let mut send_recv = self.state.send_recv;
            let in_flight = in_flight.clone();
            let responsible_sequence_nos = responsible_sequence_nos.clone();
            let decoder_area = decoder_area.clone();
            let mut tail_fbu_timeout: JoinHandle<u64> =
                tokio::task::spawn(futures::future::pending());

            let mut sequence_no = 0_u64;

            let ct = self.state.ct.clone();
            tokio::spawn(async move {
                let mut dispatch_stream = ReceiverStream::new(dispatch_rx).fuse();

                'select: loop {
                    tokio::select! {
                        biased;
                        _ = ct.cancelled() => {
                            log::info!("Dispatch task cancelled");
                            break Ok(());
                        }
                        mc_msg_res = send_recv.receive() => {
                            match mc_msg_res {
                                Ok(mm::ClientToServerMessage::MediaKeepalive(mk)) => {
                                    log::trace!("Received and ignored {:?}", mk);
                                }
                                Ok(msg) => {
                                    log::warn!("Received unexpected message on media channel: {:?}", msg);
                                }
                                Err(err) => {
                                    log::error!("Failed to receive message on media channel: {:?}", err);
                                }
                            }
                        }
                        Some(mut updates) = dispatch_stream.next() => {
                            while let Some(update) = updates.pop() {
                                let mut next = update;
                                let mut rus: Vec<mm::RectangleUpdate> = vec![];
                                'collector: loop {
                                    let next_len = rus.iter()
                                        .chain(std::iter::once(&next))
                                        .map(|ru| ru.rectangle.data.len())
                                        .sum::<usize>();
                                    if next_len <= MAX_BYTES_PER_MESSAGE {
                                       rus.push(next);
                                    } else {
                                       updates.push(next);
                                       break 'collector;
                                    }

                                    next = match updates.pop() {
                                        Some(ru) => ru,
                                        None => break 'collector,
                                    };
                                }

                                let mut in_flight_locked = in_flight.lock();
                                let mut responsible_sequence_nos_locked = responsible_sequence_nos.lock();
                                let mut old_sequence_nos = LinearSet::new();
                                for ru in &mut rus {
                                    let mm::RectangleUpdate { ref mut sequence_id, ref location, ref rectangle, ..} = ru;

                                    *sequence_id = SequenceId(sequence_no);
                                    let dimension = rectangle.dimension.as_ref().unwrap_or(&decoder_area.dimension);
                                    let origin = Point::new(location.x_position as usize, location.y_position as usize);
                                    let size = Size::new(dimension.width as usize, dimension.height as usize);
                                    let shape = s![
                                            origin.x..origin.x + size.width,
                                            origin.y..origin.y + size.height,
                                        ];

                                    old_sequence_nos.extend(responsible_sequence_nos_locked.slice(shape));
                                    responsible_sequence_nos_locked.slice_mut(shape).assign(&arr0(sequence_no));

                                    in_flight_locked.insert(sequence_no, (origin, size));
                                    sequence_no += 1;
                                }

                                for old_sequence_no in old_sequence_nos {
                                    if let Entry::Occupied(entry) = in_flight_locked.entry(old_sequence_no) {
                                        let (origin, size) = entry.get();
                                        let shape = s![origin.x..origin.x + size.width, origin.y..origin.y+size.height];
                                        if responsible_sequence_nos_locked
                                            .slice(shape)
                                            .iter()
                                            .all(|responsible| responsible != &old_sequence_no)
                                        {
                                            entry.remove();
                                        }
                                    }
                                }
                                MutexGuard::unlock_fair(responsible_sequence_nos_locked);
                                MutexGuard::unlock_fair(in_flight_locked);
                                let rus_count = rus.len();
                                let message = mm::ServerToClientMessage::new_framebuffer_update(mm::FramebufferUpdate::new(rus));
                                if let Err(e) = send_recv.send(message).await {
                                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                                        if let Some(raw_os_error) = io_err.raw_os_error() {
                                            if raw_os_error == 90 { /* Message too big */
                                                log::warn!("Message too big");
                                                deq_group!(DISPATCH_QUEUE, id: decoder_area.decoder.port, rus_count);
                                                continue;
                                            }
                                        }
                                    } else {
                                        deq_group!(DISPATCH_QUEUE, id: decoder_area.decoder.port, rus_count);
                                        break 'select Err(e);
                                    }
                                } else {
                                    // Each RectangleUpdate is one "packet sent"
                                    aperturec_metrics::builtins::packet_sent(rus_count);
                                    deq_group!(DISPATCH_QUEUE, id: decoder_area.decoder.port, rus_count);
                                    const TAIL_FBU_RTT: Duration = Duration::from_millis(100);
                                    tail_fbu_timeout = tokio::spawn(async move {
                                        tokio::time::sleep(TAIL_FBU_RTT).await;
                                        sequence_no - 1
                                    });
                                }
                            }
                        }
                        Ok(timed_out_seq_no) = &mut tail_fbu_timeout, if !tail_fbu_timeout.is_finished() => {
                            synthetic_missed_frame_tx.send(SequenceId(timed_out_seq_no))
                                .expect("Failed to generated synthetic MFR");
                        }
                        else => break Err(anyhow!("All futures exhaused in dispatch task"))
                    }
                }
            })
        };

        let mut encoding_subtask: JoinHandle<anyhow::Result<()>> = {
            let cached_fb = cached_fb.clone();
            let mut dirty_fb_watch = cached_fb.lock().dirty_watch();
            let ct = self.state.ct.clone();

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = ct.cancelled() => {
                            log::debug!("encoding subtask cancelled");
                            break Ok(());
                        }
                        (Ok(permit), ..) = join(dispatch_tx.reserve(), dirty_fb_watch.changed()) => {
                            let mut cached_fb_locked = cached_fb.lock();
                            cached_fb_locked.flip();
                            let updates = cached_fb_locked
                                .requires_updates()
                                .flat_map(|(damage_origin, data)| {
                                    encode(self.state.codec, data, MAX_BYTES_PER_MESSAGE)
                                        .map(move |(bytes, damage_relative_loc, dim, codec)| {
                                            let encoder_relative_loc = damage_relative_loc + damage_origin.to_vector();
                                            (bytes, encoder_relative_loc, dim, codec)
                                        })
                                    })
                                .map(|(bytes, encoder_relative_loc, dim, codec)| {
                                    mm::RectangleUpdate::new(
                                        SequenceId::new(0),
                                        Location::new(encoder_relative_loc.x as u64, encoder_relative_loc.y as u64),
                                        mm::Rectangle::new(
                                                codec,
                                                bytes,
                                                Some(Dimension::new(dim.width as u64, dim.height as u64)),
                                        ))
                                })
                                .collect::<Vec<_>>();
                            MutexGuard::unlock_fair(cached_fb_locked);

                            enq_group!(DISPATCH_QUEUE, id: decoder_area.decoder.port, updates.len());
                            permit.send(updates);
                        }
                    }
                }
            })
        };

        let ct = self.state.ct.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    ack_subtask_res = &mut ack_subtask, if !ack_subtask.is_finished() => {
                        let err = match ack_subtask_res {
                            Ok(Err(e)) => e,
                            Err(e) => e.into(),
                            _ => continue,
                        };
                        if !ct.is_cancelled() {
                            log::error!("ACK exited with error: {}", err);
                            ct.cancel();
                        }
                    }
                    damage_subtask_res = &mut damage_subtask, if !damage_subtask.is_finished() => {
                        let err = match damage_subtask_res {
                            Ok(Err(e)) => e,
                            Err(e) => e.into(),
                            _ => continue,
                        };
                        if !ct.is_cancelled() {
                            log::error!("DAMAGE exited with error: {}", err);
                            ct.cancel();
                        }
                    }
                    dispatch_subtask_res = &mut dispatch_subtask, if !dispatch_subtask.is_finished() => {
                        let err = match dispatch_subtask_res {
                            Ok(Err(e)) => e,
                            Err(e) => e.into(),
                            _ => continue,
                        };
                        if !ct.is_cancelled() {
                            log::error!("DISPATCH exited with error: {}", err);
                            ct.cancel();
                        }
                    }
                    encoding_subtask_res = &mut encoding_subtask, if !encoding_subtask.is_finished() => {
                        let err = match encoding_subtask_res {
                            Ok(Err(e)) => e,
                            Err(e) => e.into(),
                            _ => continue,
                        };
                        if !ct.is_cancelled() {
                            log::error!("ENCODING exited with error: {}", err);
                            ct.cancel();
                        }
                    }
                    else => break Ok(()),
                }
            }
        });

        Ok(Encoder {
            state: Running {
                task,
                ct: self.state.ct,
            },
        })
    }
}

impl Encoder<Running> {
    pub fn stop(&self) {
        self.state.ct.cancel();
    }
}

#[async_trait]
impl TryTransitionable<Terminated, Terminated> for Encoder<Running> {
    type SuccessStateful = Encoder<Terminated>;
    type FailureStateful = Encoder<Terminated>;
    type Error = anyhow::Error;

    async fn try_transition(
        self,
    ) -> Result<Self::SuccessStateful, Recovered<Self::FailureStateful, Self::Error>> {
        let res_res = self.state.task.await;
        let stateful = Encoder {
            state: Terminated {},
        };
        match res_res {
            Ok(Ok(())) => {
                log::info!("Encoder exited successfully");
                Ok(stateful)
            }
            Ok(Err(e)) => Err(Recovered {
                stateful,
                error: anyhow!("Encoder exited with internal error {}", e),
            }),
            Err(e) => Err(Recovered {
                stateful,
                error: anyhow!("Encoder exited with joining error {}", e),
            }),
        }
    }
}

impl Transitionable<Terminated> for Encoder<Running> {
    type NextStateful = Encoder<Terminated>;

    fn transition(self) -> Self::NextStateful {
        Encoder {
            state: Terminated {},
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Read;

    const DATA: [RawPixel; 4096] = [RawPixel {
        blue: 0,
        green: 255,
        red: 0,
        alpha: 255,
    }; 4096];

    #[test]
    fn simple_encode_raw() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let vec: Vec<_> = encode(Codec::new_raw(), raw_data, MAX_BYTES_PER_MESSAGE).collect();
        assert_eq!(vec.len(), 10);
        let enc_data: Vec<_> = vec.into_iter().flat_map(|(curr, ..)| curr).collect();
        for (i, chunk) in enc_data.chunks(3).enumerate() {
            assert_eq!(chunk[0], DATA[i].blue);
            assert_eq!(chunk[1], DATA[i].green);
            assert_eq!(chunk[2], DATA[i].red);
        }
    }

    #[test]
    fn simple_encode_zlib() {
        let raw_data = ArrayView2::from_shape((64, 64), &DATA).expect("create raw_data");
        let vec: Vec<_> = encode(Codec::new_zlib(), raw_data, MAX_BYTES_PER_MESSAGE).collect();
        assert_eq!(vec.len(), 2);

        let dec_data: Vec<_> = vec
            .into_iter()
            .flat_map(|(curr, _, size, _)| {
                let mut decoder = flate2::bufread::ZlibDecoder::new_with_decompress(
                    &curr[..],
                    flate2::Decompress::new(false),
                );
                let mut decomp_data = vec![];
                decoder.read_to_end(&mut decomp_data).expect("decode");
                Vec::from(&decomp_data[..size.width * size.height * ENC_BYTES_PER_PIXEL])
            })
            .collect();

        for (i, chunk) in dec_data.chunks(3).enumerate() {
            assert_eq!(chunk[0], DATA[i].blue);
            assert_eq!(chunk[1], DATA[i].green);
            assert_eq!(chunk[2], DATA[i].red);
        }
    }
}
