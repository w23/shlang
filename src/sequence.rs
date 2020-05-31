use {
	core::num::Wrapping,
	std::cmp::{Ordering, PartialEq, PartialOrd},
};

#[derive(Clone, Copy, Debug)]
pub struct Sequence {
	seq: Wrapping<u32>,
}

// TODO : wrapped order, difference, ..

impl Sequence {
	pub fn new() -> Sequence {
		Sequence { seq: Wrapping(0) }
	}

	pub fn next(&mut self) -> u32 {
		let ret = self.seq;
		self.seq += Wrapping(1);
		ret.0
	}

	pub fn age(&self, b: u32) -> Option<u32> {
		let a = self.seq.0;
		if a < b {
			if b - a < std::u32::MAX / 2 {
				return Some(b - a);
			} else {
				return None;
			}
		} else {
			if b == a {
				return Some(0);
			}
			if a - b < std::u32::MAX / 2 {
				return Some(a - b);
			} else {
				return None;
			}
		}
	}
}
// impl PartialEq<u32> for Sequence {
// 	fn eq(&self, other: &u32) -> bool {
// 		self.seq.0 == *other
// 	}
// }
//
// impl PartialOrd<u32> for Sequence {
// 	fn partial_cmp(&self, other: &u32) -> Option<Ordering> {
// 		let a = self.seq.0;
// 		let b = *other;
// 		if a < b {
// 			if a - b < std::u32::MAX / 2 {
// 				return Some(Ordering::Less);
// 			} else {
// 				return Some(Ordering::Greater);
// 			}
// 		} else {
// 			if b == a {
// 				return Some(Ordering::Equal);
// 			}
// 			if b - a < std::u32::MAX / 2 {
// 				return Some(Ordering::Greater);
// 			} else {
// 				return Some(Ordering::Less);
// 			}
// 		}
// 	}
// }

impl From<Sequence> for u32 {
	fn from(sequence: Sequence) -> u32 {
		return sequence.seq.0;
	}
}
