use {
    core::num::Wrapping,
    std::{
        //os::unix::io::AsRawFd,
        cmp::Ordering,
        io::{Cursor, Read, Write, IoSlice, IoSliceMut},
        collections::{BTreeSet},
        vec::Vec,
    },
    //log::{info, trace, warn, error, debug},
    circbuf::CircBuf,
    byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt},
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
            Ok(read) => {
                buffer.advance_write(read);
                Ok(read)
            },
            Ok(0) => {
                self.readable = false;
                Ok(0)
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

#[derive(Debug, Eq, Clone)]
struct Fragment {
    offset: u64, // global stream offset
    size: usize,
}

impl Ord for Fragment {
    fn cmp(&self, other: &Self) -> Ordering {
        self.offset.cmp(&other.offset)
    }
}

impl PartialOrd for Fragment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.offset.cmp(&other.offset))
    }
}

impl PartialEq for Fragment {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset
    }
}

pub struct Packetizer {
    buffer: CircBuf,
    packet_seq: Sequence,
    fragments: BTreeSet<Fragment>,
    buffered_bytes: u64,
}

fn write_from_offset<T: Write>(buffer: &mut /*FIXME WHY mut*/ CircBuf, offset: usize, dest: &mut T) -> std::io::Result<usize> {
    if buffer.len() == 0 { return Ok(0) }
    let bufs = buffer.get_bytes();
    let len0 = bufs[0].len();
    let len1 = bufs[1].len();
    let off1 = std::cmp::max(if offset < len0 { 0 } else { offset - len0 }, len1);
    let bufs = [
        IoSlice::new(&bufs[0][std::cmp::min(offset,len0)..len0]),
        IoSlice::new(&bufs[1][off1..len1])];
    dest.write_vectored(&bufs)
}

impl Packetizer {
    pub fn new(buffer_capacity: usize) -> Packetizer {
        Packetizer {
            buffer: CircBuf::with_capacity(buffer_capacity).unwrap(),
            packet_seq: Sequence::new(),
            fragments: BTreeSet::new(),
            buffered_bytes: 0,
        }
    }

    pub fn write_from(&mut self, source: &mut dyn CircRead) -> std::io::Result<usize> {
        let read = source.read(&mut self.buffer)?;

        if self.fragments.is_empty() {
            self.fragments.insert(Fragment{offset: self.buffered_bytes, size: read});
        } else {
            // FIXME what?! measure perf
            let mut fragment = self.fragments.iter().next_back().unwrap().clone();
            self.fragments.remove(&fragment);
            fragment.size += read;
            self.fragments.insert(fragment);
        }

        self.buffered_bytes += read as u64;
        Ok(read)
    }

    pub fn generate(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut left = buf.len() - 4;
        let mut frags = Vec::<Fragment>::new();

        for fragment in &self.fragments {
            if left < 7 || frags.len() > 15 { break; }
            left -= 6; // offset + size

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
            let offset = /* FIXME!!!! */ frag.offset as usize;
            write_from_offset(&mut self.buffer, offset, &mut wr).unwrap();
        }

        Ok(wr.position() as usize)
    }

    pub fn confirm_read(&mut self, offset: u64) -> std::io::Result<()> {
        unimplemented!()
    }

    pub fn resend(&mut self, offset: u64, size: usize) -> std::io::Result<()> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_packet_formation() {
        let data =  b"kek";
        let mut source = ReadPipe::new(data.as_ref());
        let mut packetizer = Packetizer::new(4096);
        assert_eq!(packetizer.write_from(&mut source).unwrap(), 3);

        let mut buffer = [0u8; 16];
        let packet_size = packetizer.generate(&mut buffer).unwrap();
        assert_eq!(packet_size, 4 + 4 + 2 + 3);

        let mut r = Cursor::new(buffer);
        let header = r.read_u32::<LittleEndian>().unwrap();
        let sequence = header >> 8;
        let num_fragments = header & 0xff;
        assert_eq!(sequence, 0);
        assert_eq!(num_fragments, 1);

        let offset = r.read_u32::<LittleEndian>().unwrap();
        let size = r.read_u16::<LittleEndian>().unwrap();
        assert_eq!(offset, 0);
        assert_eq!(size, 3);

        assert_eq!(data[..], buffer[r.position() as usize..packet_size]);

        // Packet format:
        // 0: u32: (24: sequence; 8: num_fragments=NF)
        // 4: u32 [NF] -- offsets (TODO: varint (masked &32) + delta-coding)
        // 4 + NF*4: u16 [NF] -- sizes (TODO: varint + delta)
        // 4 + NF*6: data0 u8[sizes[0]]
        // 4 + NF*6 + sizes[0]: data1 u8[sizes[1]]
        // ...
    }
}
