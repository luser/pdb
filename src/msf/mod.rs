// Copyright 2017 pdb Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use common::*;
use source::*;

mod page_list;
use self::page_list::PageList;

use std::fmt;

type PageNumber = u32;

#[derive(Debug,Copy,Clone)]
struct Header {
    page_size: usize,
    maximum_valid_page_number: PageNumber,
}

impl Header {
    fn pages_needed_to_store(&self, bytes: usize) -> usize {
        (bytes + (self.page_size - 1)) / self.page_size
    }

    fn validate_page_number(&self, page_number: u32) -> Result<PageNumber> {
        if page_number == 0 || page_number > self.maximum_valid_page_number {
            Err(Error::PageReferenceOutOfRange(page_number))
        } else {
            Ok(page_number as PageNumber)
        }
    }
}

#[derive(Debug)]
struct BigMSF<'s, S> {
    header: Header,
    source: S,
    stream_table: StreamTable<'s>,
}

/// Represents a stream table at various stages of access
#[doc(hidden)]
#[derive(Debug)]
enum StreamTable<'s> {
    /// The MSF header gives us the size of the table in bytes, and the list of pages (usually one)
    /// where we can find the list of pages that contain the stream table.
    HeaderOnly { size_in_bytes: usize, stream_table_location_location: PageList },

    /// Given the HeaderOnly information, we can do an initial read to get the actual location of
    /// the stream table as a PageList.
    TableFound { stream_table_location: PageList },

    // Given the table location, we can access the stream table itself
    Available { stream_table_view: Box<SourceView<'s>> }
}

const BIG_MSF_HEADER: &'static [u8] = b"Microsoft C/C++ MSF 7.00\r\n\x1a\x44\x53\x00\x00\x00";

fn view<'s>(source: &mut Source<'s>, page_list: &PageList) -> Result<Box<SourceView<'s>>> {
    // view it
    let view = source.view(page_list.source_slices())?;

    // double check our Source
    // if the Source didn't return the requested bits, that's an implementation bug, so
    // assert instead of returning an error
    assert_eq!(view.as_slice().len(), page_list.len());

    // done
    Ok(view)
}

impl<'s, S: Source<'s>> BigMSF<'s, S> {
    fn new(source: S, header_view: Box<SourceView>) -> Result<BigMSF<'s, S>> {
        let mut header = ParseBuffer::from(header_view.as_slice());

        let expected_header = header.take(BIG_MSF_HEADER.len())?;
        if expected_header != BIG_MSF_HEADER {
            return Err(Error::UnrecognizedFileFormat);
        }

        let page_size = header.parse_u32()?;
        if page_size.count_ones() != 1 || page_size < 0x100 || page_size > (128 * 0x10000) {
            return Err(Error::InvalidPageSize(page_size));
        }

        let _ = header.parse_u32()?; // free pgae map size
        let maximum_valid_page_number = header.parse_u32()?;
        let size_of_stream_table_in_bytes = header.parse_u32()?;
        let _ = header.parse_u32()?; // reserved

        let header_object = Header{
            page_size: page_size as usize,
            maximum_valid_page_number: maximum_valid_page_number,
        };

        // calculate how many pages are needed to store the stream table
        let size_of_stream_table_in_pages = header_object.pages_needed_to_store(size_of_stream_table_in_bytes as usize);

        // now: how many pages are needed to store the list of pages that store the stream table?
        // each page entry is a u32, so multiply by four
        let size_of_stream_table_page_list_in_pages = header_object.pages_needed_to_store(size_of_stream_table_in_pages * 4);

        // read the list of stream table page list pages, which immediately follow the header
        // yes, this is a stupid level of indirection
        let mut stream_table_page_list_page_list = PageList::new(header_object.page_size);
        for _ in 0..size_of_stream_table_page_list_in_pages {
            let n = header.parse_u32()?;
            stream_table_page_list_page_list.push(header_object.validate_page_number(n)?);
        }

        // truncate the stream table location location to the correct size
        stream_table_page_list_page_list.truncate(size_of_stream_table_in_pages * 4);

        Ok(BigMSF{
            header: header_object,
            source: source,
            stream_table: StreamTable::HeaderOnly {
                size_in_bytes: size_of_stream_table_in_bytes as usize,
                stream_table_location_location: stream_table_page_list_page_list,
            },
        })
    }

    fn find_stream_table(&mut self) -> Result<()> {
        let mut new_stream_table: Option<StreamTable> = None;

        if let StreamTable::HeaderOnly { size_in_bytes, ref stream_table_location_location } = self.stream_table {
            // the header indicated we need to read size_in_pages page numbers from the
            // specified PageList.

            // ask to view the location location
            let location_location = view(&mut self.source, stream_table_location_location)?;

            // build a PageList
            let mut page_list = PageList::new(self.header.page_size);
            let mut buf = ParseBuffer::from(location_location.as_slice());
            while buf.len() > 0 {
                let n = buf.parse_u32()?;
                page_list.push(self.header.validate_page_number(n)?);
            }

            page_list.truncate(size_in_bytes);

            // remember what we learned
            new_stream_table = Some(StreamTable::TableFound {
                stream_table_location: page_list,
            });
        }

        if let Some(st) = new_stream_table {
            self.stream_table = st;
        }

        Ok(())
    }

    fn make_stream_table_available(&mut self) -> Result<()> {
        // do the initial read if we must
        if let StreamTable::HeaderOnly { .. } = self.stream_table {
            self.find_stream_table()?;
        }

        // do we need to map the stream table itself?
        let mut new_stream_table: Option<StreamTable> = None;
        if let StreamTable::TableFound { ref stream_table_location } = self.stream_table {
            // yep
            // ask the source to view it
            let stream_table_view = view(&mut self.source, stream_table_location)?;

            // done
            new_stream_table = Some(StreamTable::Available {
                stream_table_view: stream_table_view,
            });
        }

        if let Some(st) = new_stream_table {
            self.stream_table = st;
        }

        // stream table is available
        assert!(match &self.stream_table {
            &StreamTable::Available { .. } => true,
            _ => false
        });

        Ok(())
    }

    fn look_up_stream(&mut self, stream_number: u32) -> Result<PageList> {
        // ensure the stream table is available
        self.make_stream_table_available()?;

        let header = self.header;

        // declare the things we're going to find
        let bytes_in_stream: u32;
        let page_list: PageList;

        if let StreamTable::Available { ref stream_table_view } = self.stream_table {
            let stream_table_slice = stream_table_view.as_slice();
            let mut stream_table = ParseBuffer::from(stream_table_slice);

            // the stream table is structured as:
            // stream_count
            // 0..stream_count: size of stream in bytes (0xffffffff indicating "stream does not exist")
            // stream 0: PageNumber
            // stream 1: PageNumber, PageNumber
            // stream 2: PageNumber, PageNumber, PageNumber, PageNumber, PageNumber
            // stream 3: PageNumber, PageNumber, PageNumber, PageNumber
            // (number of pages determined by number of bytes)

            let stream_count = stream_table.parse_u32()?;

            // check if we've already outworn our welcome
            if stream_number >= stream_count {
                return Err(Error::StreamNotFound(stream_number))
            }

            // we now have {stream_count} u32s describing the length of each stream

            // walk over the streams before the requested stream
            // we need to pay attention to how big each one is, since their page numbers come
            // before our page numbers in the stream table
            let mut page_numbers_to_skip: usize = 0;
            for _ in 0..stream_number {
                let bytes = stream_table.parse_u32()?;
                if bytes == 0xffffffff {
                    // stream is not present, ergo nothing to skip
                } else {
                    page_numbers_to_skip += header.pages_needed_to_store(bytes as usize);
                }
            }

            // read our stream's size
            bytes_in_stream = stream_table.parse_u32()?;
            if bytes_in_stream == 0xffffffff {
                return Err(Error::StreamNotFound(stream_number))
            }
            let pages_in_stream = header.pages_needed_to_store(bytes_in_stream as usize);

            // skip the remaining streams' byte counts
            let _ = stream_table.take((stream_count - stream_number - 1) as usize * 4)?;

            // skip the preceding streams' page numbers
            let _ = stream_table.take((page_numbers_to_skip as usize) * 4)?;

            // we're now at the list of pages for our stream
            // accumulate them into a PageList
            let mut list = PageList::new(header.page_size);
            for _ in 0..pages_in_stream {
                let page_number = stream_table.parse_u32()?;
                list.push(self.header.validate_page_number(page_number)?);
            }

            // truncate to the size of the stream
            list.truncate(bytes_in_stream as usize);

            page_list = list;
        } else {
            unreachable!();
        }

        // done!
        Ok(page_list)
    }
}

impl<'s, S: Source<'s>> MSF<'s, S> for BigMSF<'s, S> {
    fn get(&mut self, stream_number: u32, limit: Option<usize>) -> Result<Stream<'s>> {
        // look up the stream
        let mut page_list = self.look_up_stream(stream_number)?;

        // apply any limits we have
        if let Some(limit) = limit {
            page_list.truncate(limit);
        }

        // now that we know where this stream lives, we can view it
        let view = view(&mut self.source, &page_list)?;

        // pack it into a Stream
        let stream = Stream {
            source_view: view,
        };

        Ok(stream)
    }
}

const SMALL_MSF_HEADER: &'static [u8] = b"Microsoft C/C++ program database 2.00\r\n\x1a\x4a\x47";

// TODO: implement SmallMSF

/// Represents a single Stream within the multi-stream file.
#[derive(Debug)]
pub struct Stream<'s> {
    source_view: Box<SourceView<'s>>,
}

impl<'s> Stream<'s> {
    #[inline]
    pub fn parse_buffer(&self) -> ParseBuffer {
        let slice = self.source_view.as_slice();
        ParseBuffer::from(slice)
    }
}

/// Provides access to a "multi-stream file", which is the container format used by PDBs.
pub trait MSF<'s, S> : fmt::Debug {
    /// Accesses a stream by stream number, optionally restricted by a byte limit.
    fn get(&mut self, stream_number: u32, limit: Option<usize>) -> Result<Stream<'s>>;
}

fn header_matches(actual: &[u8], expected: &[u8]) -> bool {
    actual.len() >= expected.len() && &actual[0..expected.len()] == expected
}

pub fn open_msf<'s, S: Source<'s> + 's>(mut source: S) -> Result<Box<MSF<'s, S> + 's>> {
    // map the header
    let mut header_location = PageList::new(4096);
    header_location.push(0);
    let header_view = view(&mut source, &header_location)?;

    // see if it's a BigMSF
    if header_matches(header_view.as_slice(), BIG_MSF_HEADER) {
        // claimed!
        let bigmsf = BigMSF::new(source, header_view)?;
        return Ok(Box::new(bigmsf))
    }

    if header_matches(header_view.as_slice(), SMALL_MSF_HEADER) {
        // sorry
        return Err(Error::UnimplementedFeature("small MSF file format"));
    }

    Err(Error::UnrecognizedFileFormat)
}

#[cfg(test)]
mod tests {

    mod header {
        use msf::Header;
        use common::Error;

        #[test]
        fn test_pages_needed_to_store() {
            let h = Header{
                page_size: 4096,
                maximum_valid_page_number: 15,
            };
            assert_eq!(h.pages_needed_to_store(0), 0);
            assert_eq!(h.pages_needed_to_store(1), 1);
            assert_eq!(h.pages_needed_to_store(1024), 1);
            assert_eq!(h.pages_needed_to_store(2048), 1);
            assert_eq!(h.pages_needed_to_store(4095), 1);
            assert_eq!(h.pages_needed_to_store(4096), 1);
            assert_eq!(h.pages_needed_to_store(4097), 2);
        }

        #[test]
        fn test_validate_page_number() {
            let h = Header{
                page_size: 4096,
                maximum_valid_page_number: 15,
            };
            assert!(match h.validate_page_number(0) { Err(Error::PageReferenceOutOfRange(0)) => true, _ => false });
            assert!(match h.validate_page_number(1) { Ok(1) => true, _ => false });
            assert!(match h.validate_page_number(2) { Ok(2) => true, _ => false });
            assert!(match h.validate_page_number(14) { Ok(14) => true, _ => false });
            assert!(match h.validate_page_number(15) { Ok(15) => true, _ => false });
            assert!(match h.validate_page_number(16) { Err(Error::PageReferenceOutOfRange(16)) => true, _ => false });
            assert!(match h.validate_page_number(17) { Err(Error::PageReferenceOutOfRange(17)) => true, _ => false });
        }
    }
}
