use {
	crate::sequence::Sequence,
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	circbuf::CircBuf,
	log::{debug, error, info, trace, warn},
	std::{
		collections::VecDeque,
		io::{Cursor, Read, Seek, SeekFrom, Write},
		time::Instant,
	},
	thiserror::Error,
};

#[derive(Error, Debug, Clone)]
enum FragmentError {
	#[error("Fragment sequence is too old, expected newer than {oldest_sequence:?}")]
	TooOld { oldest_sequence: u32 },
	#[error("Fragment sequence is too new, expected older than {newest_sequence:?}")]
	TooNew { newest_sequence: u32 },
	#[error("Fragment already received")]
	AlreadyReceived,
	#[error("Fragment already received and has size that differs {known_size:?}")]
	Inconsistent { known_size: u8 },
}

#[derive(Debug, Clone)]
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
	//size: usize, // real size, not - 1
	size: u8, // size - 1
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
				size,
				data_block_index,
			}) => {
				let sequence = self.sequence;
				self.sequence += 1;
				Some(ReceivedFragment {
					size: *size,
					data_block_index: *data_block_index,
					sequence,
				})
			}
		}
	}
}

#[derive(Eq, PartialEq, Debug, Clone)]
struct MissingFragment {
	sequence: u32,
}

struct MissingFragmentIterator<'a> {
	fragments: std::iter::Rev<std::collections::vec_deque::Iter<'a, Fragment>>,
	sequence: u32,
}

impl<'a> MissingFragmentIterator<'a> {
	fn new(fragments: &'a Fragments) -> Self {
		Self {
			fragments: fragments.fragments.iter().rev(),
			// FIXME wrapping
			sequence: fragments.next_expected_sequence - fragments.fragments.len() as u32,
		}
	}
}

impl<'a> Iterator for MissingFragmentIterator<'a> {
	type Item = MissingFragment;

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			match self.fragments.next() {
				None => return None,
				Some(Fragment::Received { .. }) => {
					self.sequence += 1;
					continue;
				}
				Some(Fragment::Unknown { .. }) => {
					let sequence = self.sequence;
					self.sequence += 1;
					return Some(MissingFragment { sequence });
				}
			}
		}
	}
}

#[derive(Debug)]
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
			let index = (self.next_expected_sequence - seq - 1) as usize;
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
		// FIXME handle wrapping
		let to_remain = if sequence >= self.next_expected_sequence {
			return Err(FragmentError::TooNew {
				newest_sequence: self.next_expected_sequence - 1,
			});
		} else {
			self.next_expected_sequence - sequence - 1
		} as usize;

		// TODO check that every fragment was really confirmed?

		if to_remain >= self.fragments.len() {
			return Ok(0);
		} else {
			let deleted = self.fragments.len() - to_remain;
			self.fragments.truncate(to_remain);
			return Ok(deleted as u32);
		};
	}

	fn iter_missing_mut(&mut self) -> MissingFragmentIterator {
		MissingFragmentIterator::new(self)
	}
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
				size: 255,
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
				size: 255,
				data_block_index: 1337,
				sequence: 0
			}
		);
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 100,
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
				size: 100,
				data_block_index: 23,
				sequence: 0
			}
		);
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 255,
				data_block_index: 1337,
				sequence: 1
			}
		);
		assert!(it.next().is_none());
	}

	#[test]
	fn basic_insert_confirm() {
		let now = Instant::now();
		let mut fragments = Fragments::new();
		fragments.insert(1, 1001, 101, now).unwrap();
		fragments.insert(0, 1000, 100, now).unwrap();
		fragments.insert(3, 1003, 103, now).unwrap();
		fragments.insert(2, 1002, 102, now).unwrap();

		assert_eq!(fragments.consume(1).unwrap(), 2);

		let mut it = fragments.iter_received();
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 102,
				data_block_index: 1002,
				sequence: 2
			}
		);
		assert_eq!(
			it.next().unwrap(),
			ReceivedFragment {
				size: 103,
				data_block_index: 1003,
				sequence: 3
			}
		);
		assert!(it.next().is_none());
	}

	#[test]
	fn basic_insert_missing() {
		let now = Instant::now();
		let mut fragments = Fragments::new();
		fragments.insert(1, 1001, 101, now).unwrap();

		let mut it = fragments.iter_missing_mut();
		assert_eq!(it.next().unwrap(), MissingFragment { sequence: 0 });
		assert!(it.next().is_none());

		fragments.insert(5, 1005, 105, now).unwrap();
		let mut it = fragments.iter_missing_mut();
		assert_eq!(it.next().unwrap(), MissingFragment { sequence: 0 });
		assert_eq!(it.next().unwrap(), MissingFragment { sequence: 2 });
		assert_eq!(it.next().unwrap(), MissingFragment { sequence: 3 });
		assert_eq!(it.next().unwrap(), MissingFragment { sequence: 4 });
		assert!(it.next().is_none());
	}

	// TODO:
	// 1. 3x fragments insert errors
	// 2.a insert out of order, confirm zero
	// 2.b --//-- insert order, confirm first
	// 3. insert, confirm
	// 4. insert, insert, confirm
}

#[derive(Debug)]
struct Blocks {
	data: Vec<u8>,
	free: Vec<u32>,
}

impl Blocks {
	// FIXME:
	//  - special type for key instead of just u32
	//  - include sequence number to ensure that key is not stale
	fn new(initial_capacity: u32) -> Blocks {
		Self {
			data: vec![0u8; initial_capacity as usize * 256],
			free: (0..initial_capacity).collect(),
		}
	}

	fn alloc(&mut self) -> Option<u32> {
		self.free.pop()
		// match self.free.pop() {
		// 	None => return None, // FIXME realloc
		// 	Some(slot) => return Some(slot),
		// }
	}

	fn free(&mut self, slot: u32) {
		self.free.push(slot);
	}

	fn get_mut(&mut self, slot: u32) -> Option<&mut [u8]> {
		// FIXME range, errors
		let begin = slot as usize * 256;
		return Some(&mut self.data[begin..begin + 256]);
	}

	fn get(&mut self, slot: u32) -> Option<&[u8]> {
		let begin = slot as usize * 256;
		return Some(&self.data[begin..begin + 256]);
	}
}

pub struct Receiver {
	packet_seq: Sequence,
	fragments: Fragments,
	blocks: Blocks,
}

impl Receiver {
	pub fn new() -> Receiver {
		Receiver {
			packet_seq: Sequence::new(),
			fragments: Fragments::new(),
			blocks: Blocks::new(16),
		}
	}

	pub fn receive_packet(&mut self, data: &[u8]) -> std::io::Result<usize> {
		let now = Instant::now(); // TODO receive as argument?

		let mut tail = Cursor::new(&data);
		loop {
			let chunk_size = tail.read_u8()? as usize + 1;
			let left = data.len() - tail.position() as usize;
			println!("{} {}", chunk_size, left);
			if left == chunk_size {
				break;
			}
			tail.seek(SeekFrom::Current(chunk_size as i64))?;
		}

		let mut received = 0 as usize;
		let mut chunks = Cursor::new(&data);
		let packet_sequence = tail.read_u32::<LittleEndian>()?;
		loop {
			let frag_seq = match tail.read_u32::<LittleEndian>() {
				Err(..) => break,
				Ok(value) => value,
			};
			let frag_size = chunks.read_u8().unwrap();

			let slot = self.blocks.alloc().unwrap(); // FIXME check
			match self.fragments.insert(frag_seq, slot as u32, frag_size, now) {
				Ok(()) => {
					let frag_size = frag_size as usize + 1;
					let slot = self.blocks.get_mut(slot).unwrap();
					slot[..frag_size].copy_from_slice(
						&data[chunks.position() as usize..chunks.position() as usize + frag_size],
					);
					received += frag_size;
				}
				Err(e) => {
					error!("{:?}", e);
					self.blocks.free(slot);
				}
			}
			chunks
				.seek(SeekFrom::Current(frag_size as i64 + 1))
				.unwrap();
		}

		return Ok(received);
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

#[cfg(test)]
mod received_tests {
	use super::*;

	#[test]
	fn basic_packet_parse() {
		let mut recv = Receiver::new();
		let mut buf = [0u8; 32];
		let mut c = Cursor::new(&mut buf[..]);

		let data = &b"SHLANG"[..];
		c.write_u8((data.len() - 1) as u8).unwrap();
		c.write(&data).unwrap();

		c.write_u8(7).unwrap();
		c.write_u32::<LittleEndian>(0).unwrap();
		c.write_u32::<LittleEndian>(0).unwrap();

		let pos = c.position() as usize;
		assert_eq!(recv.receive_packet(&buf[..pos]).unwrap(), data.len());
	}
}
