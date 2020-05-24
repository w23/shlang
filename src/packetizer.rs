use {
    core::num::Wrapping,
    std::{
        //os::unix::io::AsRawFd,
        // cmp::Ordering,
        // collections::{BTreeSet},
        io::{Cursor, Read, Write, IoSlice, IoSliceMut},
        ops::{Range, RangeBounds, Bound},
        vec::Vec,
    },
    //log::{info, trace, warn, error, debug},
    circbuf::CircBuf,
    byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
    ranges::{GenericRange, Ranges},
};

pub trait CircRead {
    fn read(&mut self, buffer: &mut CircBuf) -> std::io::Result<usize>;
}

pub struct ReadPipe<T: Read> {
    pipe: T,
    readable: bool,
}

impl<T: Read> ReadPipe<T> {
    pub fn new(pipe: T) -> ReadPipe<T> {
        ReadPipe{pipe, readable: false }
    }
}

impl<T: Read> CircRead for ReadPipe<T> {
    fn read(&mut self, buffer: &mut CircBuf) -> std::io::Result<usize> {
        if buffer.cap() == 0 { return Ok(0) }
        let [buf1, buf2] = buffer.get_avail();
        let mut bufs = [IoSliceMut::new(buf1), IoSliceMut::new(buf2)];
        match self.pipe.read_vectored(&mut bufs) {
            Ok(0) => {
                self.readable = false;
                Ok(0)
            },
            Ok(read) => {
                buffer.advance_write(read);
                Ok(read)
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.readable = false;
                Ok(0)
            },
            Err(e) => Err(e),
        }
    }
}

struct Sequence {
    seq: Wrapping<u32>,
}

impl Sequence {
    fn new() -> Sequence {
        Sequence { seq: Wrapping(0) }
    }

    fn next(&mut self) -> u32 {
        let ret = self.seq;
        self.seq += Wrapping(1);
        ret.0
    }
}

pub struct Packetizer {
    buffer: CircBuf,
    packet_seq: Sequence,
    fragments: Ranges<u64>,
    ack_bytes: u64, // amount of bytes acknowledged by the receiver
}

fn write_from_offset<T: Write>(buffer: &mut /*FIXME WHY mut*/ CircBuf, offset: usize, dest: &mut T) -> std::io::Result<usize> {
    if buffer.len() == 0 { return Ok(0) }
    let bufs = buffer.get_bytes();
    let len0 = bufs[0].len();
    let len1 = bufs[1].len();
    let off1 = std::cmp::min(if offset < len0 { 0 } else { offset - len0 }, len1);
    let bufs = [
        IoSlice::new(&bufs[0][std::cmp::min(offset, len0) .. len0]),
        IoSlice::new(&bufs[1][off1 .. len1])];
    dest.write_vectored(&bufs)
}

#[derive(Clone, Debug)]
struct Fragment {
    offset: u64, // offset from start of the data transfer
    size: usize,
}

impl From<&GenericRange<u64>> for Fragment {
    fn from(range: &GenericRange<u64>) -> Fragment {
        match (range.start_bound(), range.end_bound()) {
            // FIXME handle: (a) u64 wraparound; (b) size being larger than usize (e.g. 32 bit)
            (Bound::Included(s), Bound::Included(e)) => Fragment{ offset: *s , size: (*e - *s + 1) as usize },
            (Bound::Included(s), Bound::Excluded(e)) => Fragment{ offset: *s, size: (*e - *s) as usize },
            (Bound::Excluded(s), Bound::Excluded(e)) => Fragment{ offset: *s+1, size: (*e - *s - 1) as usize },
            (Bound::Excluded(s), Bound::Included(e)) => Fragment{ offset: *s+1, size: (*e - *s) as usize },
            _ => { panic!("Unbounded range to fragment does not make any sense") }
        }
    }
}

impl Packetizer {
    pub fn new(buffer_capacity: usize) -> Packetizer {
        Packetizer {
            buffer: CircBuf::with_capacity(buffer_capacity).unwrap(),
            packet_seq: Sequence::new(),
            fragments: Ranges::new(),
            ack_bytes: 0,
        }
    }

    pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
        let begin = self.ack_bytes + self.buffer.len() as u64;
        let read = source.read(&mut self.buffer)?;
        let range = begin .. begin + read as u64;
        println!("{:?}, {:?}", self.fragments, read);
        self.fragments.insert(range);
        println!("{:?}", self.fragments);
        Ok(read)
    }

    pub fn generate(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.len() < 4 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                format!("Buffer size {} is too small", buf.len())));
        }

        // TODO: Err() no data to send

        let mut left = buf.len() - 4;
        let mut frags = Vec::<Fragment>::new();

        for fragment in self.fragments.as_slice() {
            if left < 7 || frags.len() > 15 { break; }
            left -= 6; // offset + size

            let fragment : Fragment = fragment.into();
            frags.push(fragment.clone());
            left -= std::cmp::min(left, fragment.size);
        }

        let sequence = self.packet_seq.next();
        let header = (sequence << 8) | frags.len() as u32;
        let buf_size = buf.len();
        let mut wr = Cursor::new(buf);
        wr.write_u32::<LittleEndian>(header).unwrap();

        for frag in &frags {
            let left = buf_size - wr.position() as usize;
            assert!(left > 0);
            wr.write_u32::<LittleEndian>((frag.offset & 0xffffffff) as u32).unwrap();
            let to_write = std::cmp::min(left, frag.size);
            assert!(to_write < 65535);
            wr.write_u16::<LittleEndian>(to_write as u16).unwrap();
            let offset = (frag.offset - self.ack_bytes) as usize;
            write_from_offset(&mut self.buffer, offset, &mut wr).unwrap();
            self.fragments.remove(frag.offset .. frag.offset + frag.size as u64);
        }

        Ok(wr.position() as usize)
    }

    pub fn confirm_read(&mut self, offset: u64) -> std::io::Result<usize> {
        // 3. offset is correct < bytes sent
        if offset > self.ack_bytes + self.buffer.len() as u64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                format!("Confirmed read offset {} exceeded theoretical limit of {}", offset, self.ack_bytes + self.buffer.len() as u64)));
        }

        if offset <= self.ack_bytes {
            return Ok(0);
        }

        let read = (offset - self.ack_bytes) as usize;

        // 2. erase any fragments before offset
        self.fragments.remove(self.ack_bytes..offset);

        // 1. move ack_bytes
        self.ack_bytes = offset;
        self.buffer.advance_read(read);
        Ok(read)
    }

    pub fn resend(&mut self, offset: u64, size: usize) -> std::io::Result<bool> {
        // 1. check validity:
        //      a. offset < self.ack_bytes ... ->
        let (offset, size) = if offset < self.ack_bytes {
            let head = self.ack_bytes - offset;
            if head < size as u64 {
                return Ok(false);
            }
            // FIXME overflow analysis
            (self.ack_bytes, size - head as usize)
        } else { (offset, size) };

        //      b. offset + size > bytes to transmit
        if offset + size as u64 > self.ack_bytes + self.buffer.len() as u64 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                format!("Resend request ({}, {}) exceeds data we might have sent ({}, {})",
                    offset, size, self.ack_bytes, self.buffer.len())));
        }

        // 2. insert
        Ok(self.fragments.insert(offset..offset + size as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_data(packetizer: &mut Packetizer, data: &[u8]) {
        let mut source = ReadPipe::new(data.as_ref());
        assert_eq!(packetizer.write_from(&mut source).unwrap(), data.len());
    }

    // Packet format:
    // 0: u32: (24: sequence; 8: num_fragments=NF)
    // 4: u32 [NF] -- offsets (TODO: varint (masked &32) + delta-coding)
    // 4 + NF*4: u16 [NF] -- sizes (TODO: varint + delta)
    // 4 + NF*6: data0 u8[sizes[0]]
    // 4 + NF*6 + sizes[0]: data1 u8[sizes[1]]
    // ...

    fn check_single_fragment_data(packetizer: &mut Packetizer, data: &[u8], sent_so_far: &mut usize, seq: u32) {
        let header_size = 4 + 4 + 2;
        let mut buffer = vec![0u8; data.len() + header_size]; // TODO: test case w/ buffer size < or > than expected size
        let packet_size = packetizer.generate(&mut buffer).unwrap();
        assert_eq!(packet_size, header_size + data.len());

        let mut r = Cursor::new(&buffer);
        let header = r.read_u32::<LittleEndian>().unwrap();
        let sequence = header >> 8;
        let num_fragments = header & 0xff;
        assert_eq!(sequence, seq);
        assert_eq!(num_fragments, 1);

        let offset = r.read_u32::<LittleEndian>().unwrap();
        let size = r.read_u16::<LittleEndian>().unwrap();
        assert_eq!(offset as usize, *sent_so_far);
        assert_eq!(size as usize, data.len());
        assert_eq!(buffer[r.position() as usize..packet_size], data[..]);
        *sent_so_far += data.len();
    }

    // TODO tests:
    // 1. ring buffer full (?! should we grow instead?)

    #[test]
    fn test_basic_packet_formation() {
        let mut sent_so_far = 0 as usize;
        let mut packetizer = Packetizer::new(32);

        let data = &b"keque...."[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0);

        let data = &b"qeqkekek"[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1);
    }

    #[test]
    fn test_basic_packet_formation_with_consume() {
        let mut sent_so_far = 0 as usize;
        let mut packetizer = Packetizer::new(16);

        let data = &b"keque...."[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0);

        packetizer.confirm_read(8);

        let data = &b"qeqkekek"[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 1);
    }

    #[test]
    fn test_basic_packet_resend() {
        let mut sent_so_far = 0 as usize;
        let mut packetizer = Packetizer::new(16);

        let data = &b"keque...."[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 0);

        packetizer.resend(3, 4).unwrap();
        sent_so_far = 3;
        check_single_fragment_data(&mut packetizer, &data[3..7], &mut sent_so_far, 1);

        packetizer.confirm_read(8);

        sent_so_far = data.len();
        let data = &b"qeqkekek"[..];
        write_data(&mut packetizer, &data[..]);
        check_single_fragment_data(&mut packetizer, &data, &mut sent_so_far, 2);
    }
}
