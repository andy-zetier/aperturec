use crate::geometry::{Box2D, Point};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Set {
    inner: Vec<Box2D>,
}

impl Set {
    pub fn with_initial_box(b: Box2D) -> Self {
        Set { inner: vec![b] }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
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
    ///   without modification
    /// - Simple case 2: `new` is completely contained within one of the existing boxes -> do
    ///   not add `new` to the set
    /// - Simple case 3: `new` completely contains all existing boxes -> remove all existing
    ///   boxes and just retain `new` in the set
    /// - Complex case: `new` intersects some number of existing boxes, but does not contain
    ///   all of them -> use the following algorithm:
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

        let mut new = vec![new];
        let mut non_overlapping = vec![];
        let mut batch = [None; 8];
        for e in &self.inner {
            while let Some(b) = new.pop() {
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
                    non_overlapping.extend(
                        batch
                            .iter()
                            .filter_map(|&b| b)
                            .filter(|b| !b.is_negative() && !b.is_empty()),
                    );
                } else {
                    non_overlapping.push(b);
                }
            }
            new.extend(non_overlapping.iter());
            non_overlapping.clear();
        }

        self.inner.extend(new);
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

impl FromIterator<Box2D> for Set {
    fn from_iter<T: IntoIterator<Item = Box2D>>(iter: T) -> Self {
        let mut set = Set::default();
        iter.into_iter().for_each(|b| set.add(b));
        set
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use test_log::test;

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

    #[test]
    fn add_third_box() {
        let mut set = Set {
            inner: vec![
                Box2D::new(Point::new(100, 100), Point::new(200, 200)),
                Box2D::new(Point::new(400, 0), Point::new(800, 600)),
            ],
        };
        let b = Box2D::new(Point::new(0, 0), Point::new(400, 600));
        set.add(b);

        assert!(set.iter().any(|b| b.contains(Point::new(0, 0))));
    }
}
