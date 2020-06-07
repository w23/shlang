use {
	crate::sequence::Sequence,
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	circbuf::CircBuf,
	std::{
		collections::VecDeque,
		io::{Cursor, Read, Write},
		time::Instant,
	},
	thiserror::Error,
};

#[derive(Error, Debug, Clone)]
enum FragmentError {
	#[error("Fragment sequence is too old, expected newer than {oldest_sequence:?}")]
	TooOld { oldest_sequence: u32 },
	#[error("Fragment already received")]
	AlreadyReceived,
	#[error("Fragment already received and has size that differs {known_size:?}")]
	Inconsistent { known_size: u8 },
}

enum Fragment {
	Unknown {
		expected_time: Instant,
	},
	Received {
		data_block_index: u32,
		size: u8, // size - 1
	},
}

#[derive(Eq, PartialEq, Debug, Clone)]
struct ReceivedFragment {
	size: usize, // real size, not - 1
	data_block_index: u32,
	sequence: u32,
}

struct ReceivedFragmentIterator<'a> {
	fragments: std::iter::Rev<std::collections::vec_deque::Iter<'a, Fragment>>,
	sequence: u32,
}

impl<'a> ReceivedFragmentIterator<'a> {
	fn new(fragments: &'a Fragments) -> Self {
		Self {
			fragments: fragments.fragments.iter().rev(),
			// FIXME wrapping
			sequence: fragments.next_expected_sequence - fragments.fragments.len() as u32,
		}
	}
}

impl<'a> Iterator for ReceivedFragmentIterator<'a> {
	type Item = ReceivedFragment;

	fn next(&mut self) -> Option<Self::Item> {
		match self.fragments.next() {
			None => None,
			Some(Fragment::Unknown { .. }) => None,
			Some(Fragment::Received {
				size: size,
				data_block_index: data_block_index,
			}) => {
				let sequence = self.sequence;
				self.sequence += 1;
				Some(ReceivedFragment {
					size: *size as usize + 1,
					data_block_index: *data_block_index,
					sequence,
				})
			}
		}
	}
}

struct Fragments {
	fragments: VecDeque<Fragment>,
	next_expected_sequence: u32, // FIXME wrapping
}

impl Fragments {
	fn new() -> Fragments {
		Fragments {
			fragments: VecDeque::new(),
			next_expected_sequence: 0,
		}
	}

	// Register fragment with sequence number, size, received at timestamp
	fn insert(
		&mut self,
		seq: u32,
		data_block_index: u32,
		size: u8, // size - 1
		timestamp: Instant,
	) -> Result<(), FragmentError> {
		// FIXME handle wrapping
		let oldest_sequence = self.next_expected_sequence - self.fragments.len() as u32;
		if oldest_sequence > seq {
			return Err(FragmentError::TooOld { oldest_sequence });
		}

		// FIXME wrapping
		if seq < self.next_expected_sequence {
			let index = self.fragments.len() - (self.next_expected_sequence - seq - 1) as usize;
			println!(
				"len={} next={} seq={}, index={}",
				self.fragments.len(),
				self.next_expected_sequence,
				seq,
				index
			);
			return match &mut self.fragments[index] {
				Fragment::Received {
					size: known_size, ..
				} => {
					if size != *known_size {
						Err(FragmentError::Inconsistent {
							known_size: *known_size,
						})
					} else {
						Err(FragmentError::AlreadyReceived)
					}
				}
				frag => {
					*frag = Fragment::Received {
						size,
						data_block_index,
					};
					Ok(())
				}
			};
		}

		// FIXME wrapping
		let to_create = seq - self.next_expected_sequence;
		// TODO OchenVecDeque: we have to insert to front, because VecDeque doesn't
		// treat front and back equally: there's a way to delete items from back (truncate),
		// but no such operation for front.
		for _ in 0..to_create {
			self.fragments.push_front(Fragment::Unknown {
				expected_time: timestamp,
			});
			self.next_expected_sequence += 1;
		}

		self.fragments.push_front(Fragment::Received {
			size,
			data_block_index,
		});
		self.next_expected_sequence += 1;
		Ok(())
	}

	// Consecutive fragments until first not received
	fn iter_received(&self) -> ReceivedFragmentIterator {
		ReceivedFragmentIterator::new(&self)
	}

	fn consume(&mut self, sequence: u32) -> Result<u32, FragmentError> {
		unimplemented!()
	}

	// fn iter_missing_mut(&mut self) -> FragmentsMissingIteratorMut {
	// 	unimplemented!()
	// }
}

#[cfg(test)]
mod fragment_tests {
	use super::*;

	#[test]
	fn basic_insert_received() {
		let now = Instant::now();
		let mut fragments = Fragments::new();
		fragments.insert(0, 1337, 255, now).unwrap();
		let mut it = fragments.iter_received();
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 256,
				data_block_index: 1337,
				sequence: 0
			}
		);
		assert!(it.next().is_none());

		fragments.insert(1, 23, 100, now).unwrap();
		let mut it = fragments.iter_received();
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 256,
				data_block_index: 1337,
				sequence: 0
			}
		);
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 101,
				data_block_index: 23,
				sequence: 1
			}
		);
		assert!(it.next().is_none());
	}

	#[test]
	fn basic_insert_out_of_order() {
		let now = Instant::now();
		let mut fragments = Fragments::new();
		fragments.insert(1, 1337, 255, now).unwrap();
		let mut it = fragments.iter_received();
		assert!(it.next().is_none());

		fragments.insert(0, 23, 100, now).unwrap();
		let mut it = fragments.iter_received();
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 101,
				data_block_index: 23,
				sequence: 0
			}
		);
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 256,
				data_block_index: 1337,
				sequence: 1
			}
		);
		assert!(it.next().is_none());
	}

	// TODO:
	// 1. 3x fragments insert errors
	// 2.a insert out of order, confirm zero
	// 2.b --//-- insert order, confirm first
	// 3. insert, confirm
	// 4. insert, insert, confirm
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

			// FIXME self.fragments.insert(offset, size as usize, now);
			// 1. read data into buffer
			// 2. do we need to prepare hint that it became readable
		}

		unimplemented!()
	}

	pub fn gen_feedback_packet(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		//for missing in self.fragments.missing() {}
		unimplemented!()
	}
}

impl Read for Receiver {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		unimplemented!()
	}
}
