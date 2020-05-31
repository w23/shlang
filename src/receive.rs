use {
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	circbuf::CircBuf,
	std::{
		io::{Cursor, Read, Write},
		ops::{Bound, Range, RangeBounds},
		time::Instant,
		vec::Vec,
	},
};

use crate::sequence::Sequence;

struct MissingFragment {
	offset: u64,
	size: usize,
	expected_time: Instant,
}

struct FragmentsIterator {}

impl Iterator for FragmentsIterator {
	type Item = MissingFragment;

	fn next(&mut self) -> Option<MissingFragment> {
		unimplemented!()
	}
}

struct Fragments {
	// ... ???
}

impl Fragments {
	fn new() -> Fragments {
		unimplemented!()
	}

	// Receive data at offset and with size size
	fn insert(&mut self, offset: u64, size: usize, timestamp: Instant) -> Option<usize> {
		unimplemented!()
	}

	fn missing(&self) -> FragmentsIterator {
		unimplemented!()
	}
}

pub struct Receiver {
	buffer: CircBuf,
	packet_seq: Sequence,
	fragments: Fragments,
}

impl Receiver {
	pub fn new(initial_capacity: usize) -> Receiver {
		Receiver {
			buffer: CircBuf::with_capacity(initial_capacity).unwrap(),
			packet_seq: Sequence::new(),
			fragments: Fragments::new(),
		}
	}

	pub fn receive_packet(&mut self, data: &[u8]) -> std::io::Result<usize> {
		if data.len() < 4 {
			return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, ""));
		}

		let now = Instant::now();

		let mut r = Cursor::new(&data);
		let header = r.read_u32::<LittleEndian>().unwrap();
		let sequence = header >> 8;
		let num_fragments = header & 0xff;

		for f in 0..num_fragments {
			let offset = r.read_u32::<LittleEndian>().unwrap();
			let size = r.read_u16::<LittleEndian>().unwrap();

			let offset = offset as u64; // FIXME proper mapping

			self.fragments.insert(offset, size as usize, now);
			// 1. read data into buffer
			// 2. do we need to prepare hint that it became readable
		}

		unimplemented!()
	}

	pub fn gen_feedback_packet(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		for missing in self.fragments.missing() {}
		unimplemented!()
	}
}

impl Read for Receiver {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		unimplemented!()
	}
}
