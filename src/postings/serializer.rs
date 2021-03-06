use Result;
use datastruct::FstMapBuilder;
use super::TermInfo;
use schema::Term;
use schema::Field;
use schema::FieldEntry;
use schema::FieldType;
use schema::Schema;
use schema::TextIndexingOptions;
use directory::WritePtr;
use compression::{NUM_DOCS_PER_BLOCK, SIMDBlockEncoder, CompositeEncoder};
use DocId;
use core::Segment;
use std::io;
use core::SegmentComponent;
use std::io::Write;
use common::VInt;
use common::BinarySerializable;


/// `PostingsSerializer` is in charge of serializing
/// postings on disk, in the 
/// * `.idx` (inverted index)
/// * `.pos` (positions file)
/// * `.term` (term dictionary)
/// 
/// `PostingsWriter` are in charge of pushing the data to the 
/// serializer.
/// 
/// The serializer expects to receive the following calls
/// in this order :
///
/// * `new_term(...)`
/// * `write_doc(...)`
/// * `write_doc(...)`
/// * `write_doc(...)`
/// * ...
/// * `close_term()`
/// * `new_term(...)`
/// * `write_doc(...)`
/// * ...
/// * `close_term()`
/// * `close()`
///
/// Terms have to be pushed in a lexicographically-sorted order.
/// Within a term, document have to be pushed in increasing order.
///
/// A description of the serialization format is 
/// [available here](https://fulmicoton.gitbooks.io/tantivy-doc/content/inverted-index.html). 
pub struct PostingsSerializer {
    terms_fst_builder: FstMapBuilder<WritePtr, TermInfo>, // TODO find an alternative to work around the "move"
    postings_write: WritePtr,
    positions_write: WritePtr,
    written_bytes_postings: usize,
    written_bytes_positions: usize,
    last_doc_id_encoded: u32,
    positions_encoder: CompositeEncoder,
    block_encoder: SIMDBlockEncoder,
    doc_ids: Vec<DocId>,
    term_freqs: Vec<u32>,
    position_deltas: Vec<u32>,
    schema: Schema,
    text_indexing_options: TextIndexingOptions,
    term_open: bool,
}

impl PostingsSerializer {
    
    /// Open a new `PostingsSerializer` for the given segment  
    pub fn open(segment: &mut Segment) -> Result<PostingsSerializer> {
        let terms_write = try!(segment.open_write(SegmentComponent::TERMS));
        let terms_fst_builder = try!(FstMapBuilder::new(terms_write));
        let postings_write = try!(segment.open_write(SegmentComponent::POSTINGS));
        let positions_write = try!(segment.open_write(SegmentComponent::POSITIONS));
        let schema = segment.schema();
        Ok(PostingsSerializer {
            terms_fst_builder: terms_fst_builder,
            postings_write: postings_write,
            positions_write: positions_write,
            written_bytes_postings: 0,
            written_bytes_positions: 0,
            last_doc_id_encoded: 0u32,
            positions_encoder: CompositeEncoder::new(),
            block_encoder: SIMDBlockEncoder::new(),
            doc_ids: Vec::new(),
            term_freqs: Vec::new(),
            position_deltas: Vec::new(),
            schema: schema,
            text_indexing_options: TextIndexingOptions::Unindexed,
            term_open: false,
        })
    }
    
    fn load_indexing_options(&mut self, field: Field) {
        let field_entry: &FieldEntry = self.schema.get_field_entry(field);
        self.text_indexing_options = match *field_entry.field_type() {
            FieldType::Str(ref text_options) => {
                text_options.get_indexing_options()
            }
            FieldType::U32(ref u32_options) => {
                if u32_options.is_indexed() {
                    TextIndexingOptions::Unindexed
                }
                else {
                    TextIndexingOptions::Untokenized    
                }
            }
        };
    }
    
    /// Starts the postings for a new term.
    /// * term - the term. It needs to come after the previous term according
    ///   to the lexicographical order. 
    /// * doc_freq - return the number of document containing the term.
    pub fn new_term(&mut self, term: &Term, doc_freq: DocId) -> io::Result<()> {
        if self.term_open {
            panic!("Called new_term, while the previous term was not closed.");
        }
        self.term_open = true;
        self.load_indexing_options(term.field());
        self.doc_ids.clear();
        self.last_doc_id_encoded = 0;
        self.term_freqs.clear();
        self.position_deltas.clear();
        let term_info = TermInfo {
            doc_freq: doc_freq,
            postings_offset: self.written_bytes_postings as u32,
            positions_offset: self.written_bytes_positions as u32,
        };
        self.terms_fst_builder
            .insert(term.as_slice(), &term_info)
    }
    
    /// Finish the serialization for this term postings.
    ///
    /// If the current block is incomplete, it need to be encoded
    /// using `VInt` encoding.  
    pub fn close_term(&mut self,) -> io::Result<()> {
        if self.term_open {
            if !self.doc_ids.is_empty() {
                // we have doc ids waiting to be written
                // this happens when the number of doc ids is 
                // not a perfect multiple of our block size.
                //
                // In that case, the remaining part is encoded
                // using variable int encoding.
                {
                    let block_encoded = self.block_encoder.compress_vint_sorted(&self.doc_ids, self.last_doc_id_encoded);
                    self.written_bytes_postings += block_encoded.len();
                    try!(self.postings_write.write_all(block_encoded));
                    self.doc_ids.clear();
                }
                // ... Idem for term frequencies 
                if self.text_indexing_options.is_termfreq_enabled() {
                    let block_encoded = self.block_encoder.compress_vint_unsorted(&self.term_freqs[..]);
                    for num in block_encoded {
                        self.written_bytes_postings += try!(num.serialize(&mut self.postings_write));
                    }
                    self.term_freqs.clear();
                }
            }
            // On the other hand, positions are entirely buffered until the
            // end of the term, at which point they are compressed and written.
            if self.text_indexing_options.is_position_enabled() {
                self.written_bytes_positions += try!(VInt(self.position_deltas.len() as u64).serialize(&mut self.positions_write));
                let positions_encoded: &[u8] = self.positions_encoder.compress_unsorted(&self.position_deltas[..]);
                try!(self.positions_write.write_all(positions_encoded));
                self.written_bytes_positions += positions_encoded.len();
                self.position_deltas.clear();
            }
            self.term_open = false;
        }
        Ok(())
    }
    
    
    /// Serialize the information that a document contains the current term,
    /// its term frequency, and the position deltas.
    ///
    /// At this point, the positions are already `delta-encoded`.
    /// For instance, if the positions are `2, 3, 17`,
    /// `position_deltas` is `2, 1, 14`
    ///
    /// Term frequencies and positions may be ignored by the serializer depending
    /// on the configuration of the field in the `Schema`.
    pub fn write_doc(&mut self, doc_id: DocId, term_freq: u32, position_deltas: &[u32]) -> io::Result<()> {
        self.doc_ids.push(doc_id);
        if self.text_indexing_options.is_termfreq_enabled() {
            self.term_freqs.push(term_freq as u32);
        }
        if self.text_indexing_options.is_position_enabled() {
            self.position_deltas.extend_from_slice(position_deltas);
        }
        if self.doc_ids.len() == NUM_DOCS_PER_BLOCK {
            {
                // encode the doc ids
                let block_encoded: &[u8] = self.block_encoder.compress_block_sorted(&self.doc_ids, self.last_doc_id_encoded);
                self.last_doc_id_encoded = self.doc_ids[self.doc_ids.len() - 1];
                try!(self.postings_write.write_all(block_encoded));
                self.written_bytes_postings += block_encoded.len();
            }
            if self.text_indexing_options.is_termfreq_enabled() {
                // encode the term_freqs
                let block_encoded: &[u8] = self.block_encoder.compress_block_unsorted(&self.term_freqs);
                try!(self.postings_write.write_all(block_encoded));
                self.written_bytes_postings += block_encoded.len();
                self.term_freqs.clear();
            }
            self.doc_ids.clear();
        }
        Ok(())
    }
    
    /// Closes the serializer.
    pub fn close(mut self,) -> io::Result<()> {
        try!(self.close_term());
        try!(self.terms_fst_builder.finish());
        try!(self.postings_write.flush());
        try!(self.positions_write.flush());
        Ok(())
    }
}
