use {
	byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
	log::{debug, error, info, trace, warn},
	std::{
		cmp::min,
		io::{Cursor, Read, Seek, SeekFrom},
		time::Instant,
	},
	thiserror::Error,
};

use crate::OchenCircusBuf::OchenCircusBuf;

#[derive(Error, Debug, Clone, PartialEq)]
enum SegmentationFault {
	#[error("Attempted to insert missing segment not at the head {head_offset:?}")]
	TooOld { head_offset: u64 },
	// #[error("Fragment sequence is too new, expected older than {newest_sequence:?}")]
	// TooNew { newest_sequence: u32 },
	// #[error("Fragment already received")]
	// AlreadyReceived,
	// #[error("Fragment already received and has size that differs {known_size:?}")]
	// Inconsistent { known_size: u8 },
}

#[derive(Debug, Clone, PartialEq)]
struct MissingSegment {
	// global offsets
	begin: u64,
	end: u64,

	// expected to have been received at this time
	timestamp: Instant,
}

struct MissingSegmentIterator<'a> {
	segments: &'a mut Vec<MissingSegment>,
	next: usize,
}

impl<'a> MissingSegmentIterator<'a> {
	fn new(segments: &'a mut Vec<MissingSegment>) -> Self {
		Self { segments, next: 0 }
	}

	fn next(&mut self) -> Option<&MissingSegment> {
		if self.next >= self.segments.len() {
			return None;
		}

		let item = &self.segments[self.next];
		self.next += 1;
		Some(item)
	}

	fn update_timestamp(&mut self, timestamp: Instant) -> Option<Instant> {
		if self.next > self.segments.len() || self.next == 0 {
			return None;
		}

		let item = &mut self.segments[self.next - 1];
		let old_timestamp = item.timestamp;
		item.timestamp = timestamp;
		Some(old_timestamp)
	}

	fn cut_range(&mut self, cut_begin: u64, cut_end: u64, timestamp: Instant) {
		// -> Result<(), SegmentationFault> {
		if self.next > self.segments.len() || self.next == 0 {
			panic!("Cutting an invalid iterator");
		}

		let item = &mut self.segments[self.next - 1];
		if item.begin > cut_begin || item.end < cut_end {
			panic!("Invalid range, should be strictly within");
		}

		if cut_begin == item.begin && cut_end == item.end {
			// Case 1: removing the entire missing segment (not missing anymore!)
			self.segments.remove(self.next - 1);
			self.next -= 1;
		} else if cut_begin == item.begin {
			// Case 2a: removing first part of missing segment
			item.begin = cut_end;
		} else if cut_end == item.end {
			// Case 2b: removing last part of missing segment
			item.end = cut_begin;
			item.timestamp = timestamp;
		} else {
			// Case 3: splitting missing segment in two
			let item_end = item.end;
			let item_timestamp = item.timestamp;
			item.timestamp = timestamp;
			item.end = cut_begin;
			self.segments.insert(
				self.next,
				MissingSegment {
					begin: cut_end,
					end: item_end,
					timestamp: item_timestamp,
				},
			);
			self.next += 1;
		}
	}
}

#[derive(Debug)]
struct MissingSegments {
	segments: Vec<MissingSegment>,
}

impl MissingSegments {
	fn new() -> Self {
		Self {
			segments: Vec::new(),
		}
	}

	// Insert new known missing segment
	// Only supports adding at the head
	fn insert(&mut self, begin: u64, end: u64, timestamp: Instant) -> Result<(), SegmentationFault> {
		assert!(begin <= end);
		match self.segments.last() {
			Some(segment) if segment.end > begin => {
				return Err(SegmentationFault::TooOld {
					head_offset: segment.end,
				})
			}
			_ => {}
		}

		self.segments.push(MissingSegment {
			begin,
			end,
			timestamp,
		});
		Ok(())
	}

	fn missing_iter(&mut self) -> MissingSegmentIterator {
		MissingSegmentIterator::new(&mut self.segments)
	}

	fn first(&self) -> Option<u64> {
		match self.segments.first() {
			Some(segment) => Some(segment.begin),
			_ => None,
		}
	}

	fn total_size(&self) -> usize {
		let mut missing_size = 0;
		for segment in &self.segments {
			missing_size += segment.end - segment.begin;
		}
		missing_size as usize
	}
}

#[cfg(test)]
mod segment_tests {
	use super::*;

	#[test]
	fn shlangobidon() {
		let now = Instant::now();
		let mut segs = MissingSegments::new();
		segs.insert(0, 7, now).unwrap();
		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 0,
				end: 7,
				timestamp: now
			}
		);
		assert!(iter.next().is_none());

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 0,
				end: 7,
				timestamp: now
			}
		);
		iter.cut_range(0, 7, now);
		assert!(iter.next().is_none());

		let mut iter = segs.missing_iter();
		assert!(iter.next().is_none());
	}

	#[test]
	fn insert_iter() {
		let now = Instant::now();
		let mut segs = MissingSegments::new();
		segs.insert(10, 20, now).unwrap();
		segs.insert(30, 40, now).unwrap();

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 10,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);
		assert!(iter.next().is_none());

		assert_eq!(
			segs.insert(23, 27, now).err().unwrap(),
			SegmentationFault::TooOld { head_offset: 40 }
		);
	}

	#[test]
	fn cutlery() {
		let now = Instant::now();
		let mut segs = MissingSegments::new();
		segs.insert(10, 20, now).unwrap();
		segs.insert(30, 40, now).unwrap();
		segs.insert(50, 80, now).unwrap();
		segs.insert(90, 100, now).unwrap();

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 10,
				end: 20,
				timestamp: now
			}
		);
		let now2 = Instant::now();
		iter.cut_range(10, 15, now2);

		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 40,
				timestamp: now
			}
		);
		let now3 = Instant::now();
		iter.cut_range(37, 40, now3);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 80,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 37,
				timestamp: now3
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 80,
				timestamp: now
			}
		);
		let now4 = Instant::now();
		iter.cut_range(60, 70, now4);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 90,
				end: 100,
				timestamp: now
			}
		);

		let mut iter = segs.missing_iter();
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 15,
				end: 20,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 30,
				end: 37,
				timestamp: now3
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 50,
				end: 60,
				timestamp: now4
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 70,
				end: 80,
				timestamp: now
			}
		);
		assert_eq!(
			iter.next().unwrap(),
			&MissingSegment {
				begin: 90,
				end: 100,
				timestamp: now
			}
		);
		assert!(iter.next().is_none());
	}
}

pub struct Receiver {
	missing: MissingSegments,
	buffer: OchenCircusBuf,
	buffer_start_offset: u64,
	stream_front: u64,
	last_sent_consumed_offset: u64,
}

impl Receiver {
	pub fn new(capacity: usize) -> Receiver {
		Receiver {
			missing: MissingSegments::new(),
			buffer: OchenCircusBuf::with_capacity(capacity),
			buffer_start_offset: 0,
			stream_front: 0,
			last_sent_consumed_offset: 0,
		}
	}

	pub fn read_chunk_segment(&mut self, chunk: &[u8], now: Instant) -> std::io::Result<usize> {
		let mut rd = Cursor::new(&chunk);
		let mut segment_begin = rd.read_u64::<LittleEndian>().unwrap();
		let mut segment_data = &chunk[8..];

		let mut segment_end = segment_begin + segment_data.len() as u64;
		if segment_end <= self.buffer_start_offset {
			// Segment is too old, skip
			return Ok(0);
		}

		if segment_begin < self.buffer_start_offset {
			let shift = (self.buffer_start_offset - segment_begin) as usize;
			segment_begin = self.buffer_start_offset;
			segment_data = &segment_data[shift..segment_data.len()];
		}

		trace!("buffer_start_offset={}", self.buffer_start_offset);
		debug!(
			"Receive segment @{}..{} ({})",
			segment_begin,
			segment_end,
			segment_data.len()
		);

		let mut missing_iter = self.missing.missing_iter();
		'missing: loop {
			let missing = match missing_iter.next() {
				Some(item) => item,
				_ => break 'missing,
			};

			trace!("  Missing {:?}", missing);

			// missing segment is fully before received segment
			if missing.end <= segment_begin {
				continue 'missing;
			}

			// Missing segment is fully after received segment
			if missing.begin >= segment_end {
				// TODO count duplicate received data
				return Ok(0);
			}

			// Handle missing & received
			// Find begin intersection
			if missing.begin > segment_begin {
				let shift = (missing.begin - segment_begin) as usize;
				segment_begin = missing.begin;
				segment_data = &segment_data[shift..segment_data.len()];
			};

			trace!(
				"    segment_begin={} len={}",
				segment_begin,
				segment_data.len()
			);

			let write_end = min(missing.end, segment_end);
			let to_write_size = (write_end - segment_begin) as usize;
			// Write received to buffer
			let buf_offset = (segment_begin - self.buffer_start_offset) as usize;

			trace!(
				"    segment_begin={} write_end={} to_write_size={} buf_offset={}",
				segment_begin,
				write_end,
				to_write_size,
				buf_offset
			);

			let written = self
				.buffer
				.write_data_at_read_offset(buf_offset, &segment_data[..to_write_size]);

			if written < to_write_size {
				// FIXME Result<Err
				debug!(
					"    segment_begin={} write_end={} to_write_size={} buf_offset={}",
					segment_begin, write_end, to_write_size, buf_offset
				);
				//debug!("");
				error!("Inconsistent missing vs buffer state. Could write only {} bytes of {} in the middle, offset = {}, global offset = {}", written, to_write_size, buf_offset, self.buffer_start_offset);
			}

			// Update missing
			missing_iter.cut_range(segment_begin, write_end, now);
			//.unwrap();

			// Update segment and early exit if empty
			let shift = (write_end - segment_begin) as usize;
			segment_begin = write_end;
			segment_data = &segment_data[shift..segment_data.len()];
			if segment_data.len() == 0 {
				return Ok(0);
			}
		}

		// handle head
		let buffer_head = self.buffer_start_offset + self.buffer.len() as u64;
		if segment_end > buffer_head {
			// Write received to buffer
			let buf_offset = (segment_begin - self.buffer_start_offset) as usize;
			let written = self
				.buffer
				.write_data_at_read_offset(buf_offset, segment_data);

			if written < segment_data.len() {
				warn!("Write buffer exhausted, could write only {} bytes of {} at offset = {}, global_offset = {}", written, segment_data.len(), buf_offset, self.buffer_start_offset);
			}
		}

		Ok(0)
	}

	pub fn update_stream_front(&mut self, stream_front: u64, now: Instant) {
		if stream_front > self.stream_front {
			self
				.missing
				.insert(self.stream_front, stream_front, now)
				.unwrap();
			self.stream_front = stream_front;
		}
	}

	pub fn make_chunk_feedback(
		&mut self,
		// FIXME older_than: Instant,
		buf: &mut [u8],
	) -> std::io::Result<usize> {
		let mut left = buf.len();

		if left < 2 + 8 {
			return Ok(0);
		}

		let mut wr = Cursor::new(buf);

		// Put chunk header placeholder
		let header: u16 = (1u16 << 11) | 0 as u16;
		wr.write_u16::<LittleEndian>(header).unwrap();
		left -= 2;

		// write consumed offset placeholder
		let mut last_consumed_offset = self.buffer_start_offset;
		wr.write_u64::<LittleEndian>(self.buffer_start_offset)
			.unwrap();
		left -= 8;

		let mut consumed_offset = None;
		let mut iter = self.missing.missing_iter();
		loop {
			let segment = match iter.next() {
				Some(segment) => segment,
				None => break,
			};

			if consumed_offset.is_none() {
				consumed_offset = Some(segment.begin);
			}

			if left < 12 {
				break;
			}

			wr.write_u64::<LittleEndian>(segment.begin).unwrap();
			let size = segment.end - segment.begin;
			wr.write_u32::<LittleEndian>(size as u32).unwrap();
			left -= 12;
		}

		let wr_end = wr.position();
		let chunk_size = wr_end - 2;

		// Write final chunk size
		wr.seek(SeekFrom::Start(0)).unwrap();
		let header: u16 = (1u16 << 11) | chunk_size as u16;
		wr.write_u16::<LittleEndian>(header).unwrap();
		if let Some(offset) = consumed_offset {
			assert!(offset >= self.buffer_start_offset);
			last_consumed_offset = offset;
			wr.write_u64::<LittleEndian>(offset).unwrap();
		}

		self.last_sent_consumed_offset = last_consumed_offset;
		Ok(wr_end as usize)
	}

	pub fn data_to_read(&self) -> usize {
		match self.missing.first() {
			Some(offset) => {
				trace!("missing first={}", offset);
				(offset - self.buffer_start_offset) as usize
			}
			None => {
				trace!("buffer len={}", self.buffer.len());
				self.buffer.len()
			}
		}
	}

	pub fn done(&self) -> bool {
		let missing_size = self.missing.total_size();
		// if there are no missing segments, buffer_start_offset is stream front
		missing_size == 0 && self.buffer_start_offset == self.last_sent_consumed_offset
	}
}

impl Read for Receiver {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let size = self.data_to_read();

		let buf_len = buf.len();
		let dest = &mut buf[..min(buf_len, size)];
		let mut written = 0;

		let [buf1, buf2] = self.buffer.get_data();

		let copy_size = min(dest.len(), buf1.len());
		dest[..copy_size].clone_from_slice(&buf1[..copy_size]);
		written += copy_size;

		let dest = &mut dest[copy_size..];
		if dest.len() > 0 {
			let copy_size = min(dest.len(), buf2.len());
			dest[..copy_size].clone_from_slice(&buf2[..copy_size]);
			written += copy_size;
		}
		self.buffer.consume(written);
		self.buffer_start_offset += written as u64;
		Ok(written)
	}
}

#[cfg(test)]
mod receiver_tests {
	use super::*;
	use std::io::Write;

	struct Segment<'a> {
		offset: u64,
		data: &'a [u8],
	}

	struct Packet<'a> {
		front: usize,
		segments: &'a [Segment<'a>],
	}

	fn push_packet(recv: &mut Receiver, pkt: &Packet) {
		let mut buf = [0u8; 1500];
		let now = Instant::now();

		recv.update_stream_front(pkt.front as u64, now);

		for seg in pkt.segments {
			let mut c = Cursor::new(&mut buf[..]);
			c.write_u64::<LittleEndian>(seg.offset).unwrap();
			assert_eq!(c.write(seg.data).unwrap(), seg.data.len());
			let size = c.position() as usize;
			recv.read_chunk_segment(&buf[..size], now).unwrap();
		}
	}

	#[test]
	fn basic_packet_parse() {
		let mut recv = Receiver::new(1024);

		let data = &b"SHLANG"[..];
		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[Segment {
					offset: 0,
					data: &data,
				}],
			},
		);

		let mut buf = [0u8; 32];
		assert_eq!(recv.read(&mut buf).unwrap(), data.len());
		assert_eq!(&buf[..data.len()], data);
	}

	#[test]
	fn basic_packet_parse_2seg() {
		let mut recv = Receiver::new(1024);

		let data = &b"SHLANGOKEFIR"[..];
		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[
					Segment {
						offset: 0,
						data: &data[..5],
					},
					Segment {
						offset: 5,
						data: &data[5..],
					},
				],
			},
		);

		let mut buf = [0u8; 32];
		assert_eq!(recv.read(&mut buf).unwrap(), data.len());
		assert_eq!(&buf[..data.len()], data);
	}

	#[test]
	fn packet_out_of_order() {
		let mut recv = Receiver::new(1024);

		let data = &b"SHLANGOBIDON"[..];
		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[Segment {
					offset: 7,
					data: &data[7..],
				}],
			},
		);

		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[Segment {
					offset: 0,
					data: &data[..7],
				}],
			},
		);

		let mut buf = [0u8; 32];
		assert_eq!(recv.read(&mut buf).unwrap(), data.len());
		assert_eq!(&buf[..data.len()], data);
	}

	#[test]
	fn packet_overlapping_missing() {
		let mut recv = Receiver::new(1024);

		let data = &b"SHLANGOBIDON"[..];
		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[Segment {
					offset: 0,
					data: &data[..7],
				}],
			},
		);

		push_packet(
			&mut recv,
			&Packet {
				front: data.len(),
				segments: &[Segment {
					offset: 4,
					data: &data[4..],
				}],
			},
		);

		let mut buf = [0u8; 32];
		assert_eq!(recv.read(&mut buf).unwrap(), data.len());
		assert_eq!(&buf[..data.len()], data);
	}
}
