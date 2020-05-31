use core::num::Wrapping;

pub struct Sequence {
	seq: Wrapping<u32>,
}

impl Sequence {
	pub fn new() -> Sequence {
		Sequence { seq: Wrapping(0) }
	}

	pub fn next(&mut self) -> u32 {
		let ret = self.seq;
		self.seq += Wrapping(1);
		ret.0
	}
}
