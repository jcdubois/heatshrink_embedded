use super::HSError;
use super::HSfinishRes;
use super::HSpollRes;
use super::HSsinkRes;
use super::OutputInfo;
use super::HEATSHRINK_INPUT_BUFFER_SIZE;
use super::HEATSHRINK_LOOKAHEAD_BITS;
use super::HEATSHRINK_WINDOWS_BITS;

#[derive(Debug, Copy, Clone, PartialEq)]
enum HSDstate {
    HSDSTagBit,          /* tag bit */
    HSDSYieldLiteral,    /* ready to yield literal byte */
    HSDSBackrefIndexMsb, /* most significant byte of index */
    HSDSBackrefIndexLsb, /* least significant byte of index */
    HSDSBackrefCountLsb, /* least significant byte of count */
    HSDSYieldBackref,    /* ready to yield back-reference */
}

/// the decoder instance
#[derive(Debug)]
pub struct HeatshrinkDecoder {
    input_size: u16,
    input_index: u16,
    output_count: u16,
    output_index: u16,
    head_index: u16,
    current_byte: u8,
    bit_index: u8,
    state: HSDstate,
    input_buffer: [u8; HEATSHRINK_INPUT_BUFFER_SIZE],
    output_buffer: [u8; 1 << HEATSHRINK_WINDOWS_BITS],
}

/// uncompress the src buffer to the destination buffer
pub fn decode<'a>(src: &[u8], dst: &'a mut [u8]) -> Result<&'a [u8], HSError> {
    let mut total_input_size = 0;
    let mut total_output_size = 0;

    let mut dec = HeatshrinkDecoder::new();

    while total_input_size < src.len() {
        let mut segment_input_size = 0;

        // Fill the input buffer from the src buffer
        match dec.sink(&src[total_input_size..], &mut segment_input_size) {
            HSsinkRes::HSRSinkOK => {
                total_input_size += segment_input_size;
            }
            HSsinkRes::HSRSinkFull => {}
            HSsinkRes::HSRSinkErrorMisuse => {
                return Err(HSError::HSErrorInternal);
            }
        }

        if total_output_size == dst.len() {
            return Err(HSError::HSErrorOutputFull);
        } else {
            // process the current input buffer
            loop {
                let mut segment_output_size = 0;

                match dec.poll(&mut dst[total_output_size..], &mut segment_output_size) {
                    HSpollRes::HSRPollMore => {
                        return Err(HSError::HSErrorOutputFull);
                    }
                    HSpollRes::HSRPollEmpty => {
                        total_output_size += segment_output_size;
                        break;
                    }
                    HSpollRes::HSRPollErrorMisuse => {
                        return Err(HSError::HSErrorInternal);
                    }
                }
            }
        }

        // if all the src buffer is processed, finish the uncompress stream
        if total_input_size == src.len() {
            match dec.finish() {
                HSfinishRes::HSRFinishDone => {}
                HSfinishRes::HSRFinishMore => {
                    return Err(HSError::HSErrorOutputFull);
                }
            }
        }
    }

    Ok(&dst[..total_output_size])
}

impl HeatshrinkDecoder {
    /// Create a new decoder instance
    pub fn new() -> Self {
        HeatshrinkDecoder {
            input_size: 0,
            input_index: 0,
            output_count: 0,
            output_index: 0,
            head_index: 0,
            current_byte: 0,
            bit_index: 0,
            state: HSDstate::HSDSTagBit,
            input_buffer: [0; HEATSHRINK_INPUT_BUFFER_SIZE],
            output_buffer: [0; 1 << HEATSHRINK_WINDOWS_BITS],
        }
    }

    /// Reset the current decoder instance
    pub fn reset(&mut self) {
        self.input_size = 0;
        self.input_index = 0;
        self.output_count = 0;
        self.output_index = 0;
        self.head_index = 0;
        self.current_byte = 0;
        self.bit_index = 0;
        self.state = HSDstate::HSDSTagBit;
        // memset self.buffer to 0
        self.input_buffer.iter_mut().for_each(|m| *m = 0);
        self.output_buffer.iter_mut().for_each(|m| *m = 0);
    }

    /// Add an input buffer to be processed/uncompressed
    pub fn sink(&mut self, input_buffer: &[u8], input_size: &mut usize) -> HSsinkRes {
        let remaining_size = self.input_buffer.len() - self.input_size as usize;

        if remaining_size == 0 {
            *input_size = 0;
            return HSsinkRes::HSRSinkFull;
        }

        let copy_size = if remaining_size < input_buffer.len() {
            remaining_size
        } else {
            input_buffer.len()
        };

        // memcpy content of input_buffer into self.input_buffer.
        self.input_buffer[self.input_size as usize..(self.input_size as usize + copy_size)]
            .copy_from_slice(&input_buffer[0..copy_size]);
        self.input_size += copy_size as u16;
        *input_size = copy_size;

        return HSsinkRes::HSRSinkOK;
    }

    /// function to process the input/internal buffer and put the uncompressed
    /// stream in the provided buffer.
    pub fn poll(&mut self, output_buffer: &mut [u8], output_size: &mut usize) -> HSpollRes {
        if output_buffer.len() == 0 {
            return HSpollRes::HSRPollErrorMisuse;
        } else {
            *output_size = 0;

            let mut output_info = OutputInfo::new(output_buffer, output_size);

            loop {
                let in_state = self.state;

                match in_state {
                    HSDstate::HSDSTagBit => {
                        self.state = self.st_tag_bit();
                    }
                    HSDstate::HSDSYieldLiteral => {
                        self.state = self.st_yield_literal(&mut output_info);
                    }
                    HSDstate::HSDSBackrefIndexMsb => {
                        self.state = self.st_backref_index_msb();
                    }
                    HSDstate::HSDSBackrefIndexLsb => {
                        self.state = self.st_backref_index_lsb();
                    }
                    HSDstate::HSDSBackrefCountLsb => {
                        self.state = self.st_backref_count_lsb();
                    }
                    HSDstate::HSDSYieldBackref => {
                        self.state = self.st_yield_backref(&mut output_info);
                    }
                }

                // If the current state cannot advance, check if input or
                // output buffer are exhausted.
                if self.state == in_state {
                    if output_info.can_take_byte() {
                        return HSpollRes::HSRPollEmpty;
                    } else {
                        return HSpollRes::HSRPollMore;
                    }
                }
            }
        }
    }

    fn st_tag_bit(&mut self) -> HSDstate {
        match self.get_bits(1) {
            None => HSDstate::HSDSTagBit,
            Some(0) => {
                self.output_index = 0;
                HSDstate::HSDSBackrefIndexLsb
            }
            Some(_) => HSDstate::HSDSYieldLiteral,
        }
    }

    fn st_yield_literal(&mut self, output_info: &mut OutputInfo) -> HSDstate {
        // Emit a repeated section from the window buffer, and add it (again)
        // to the window buffer. (Note that the repetition can include itself)
        if output_info.can_take_byte() {
            match self.get_bits(8) {
                None => HSDstate::HSDSYieldLiteral, // input_buffer is consumed
                Some(x) => {
                    let c: u8 = (x & 0xff) as u8;
                    let mask = self.output_buffer.len() - 1;
                    self.output_buffer[self.head_index as usize & mask] = c;
                    self.head_index += 1;
                    output_info.push_byte(c);
                    HSDstate::HSDSTagBit
                }
            }
        } else {
            return HSDstate::HSDSYieldLiteral;
        }
    }

    fn st_backref_index_msb(&mut self) -> HSDstate {
        match self.get_bits(0) {
            None => HSDstate::HSDSBackrefIndexMsb,
            Some(x) => {
                self.output_index = x << 8;
                HSDstate::HSDSBackrefIndexLsb
            }
        }
    }

    fn st_backref_index_lsb(&mut self) -> HSDstate {
        match self.get_bits(8) {
            None => HSDstate::HSDSBackrefIndexLsb,
            Some(x) => {
                self.output_index |= x;
                self.output_index += 1;
                self.output_count = 0;
                HSDstate::HSDSBackrefCountLsb
            }
        }
    }

    fn st_backref_count_lsb(&mut self) -> HSDstate {
        match self.get_bits(HEATSHRINK_LOOKAHEAD_BITS as u8) {
            None => HSDstate::HSDSBackrefCountLsb,
            Some(x) => {
                self.output_count |= x;
                self.output_count += 1;
                HSDstate::HSDSYieldBackref
            }
        }
    }

    fn st_yield_backref(&mut self, output_info: &mut OutputInfo) -> HSDstate {
        if output_info.can_take_byte() {
            let mut i: usize = 0;
            let mut count = output_info.remaining_free_size();
            let mask = self.output_buffer.len() - 1;

            if usize::from(self.output_count) < count {
                count = usize::from(self.output_count);
            }

            while i < count {
                let c = if self.output_index > self.head_index {
                    0
                } else {
                    self.output_buffer[(self.head_index - self.output_index) as usize & mask]
                };
                self.output_buffer[self.head_index as usize & mask] = c;
                output_info.push_byte(c);
                self.head_index += 1;
                i += 1;
            }

            self.output_count -= count as u16;

            if self.output_count == 0 {
                return HSDstate::HSDSTagBit;
            }
        }
        return HSDstate::HSDSYieldBackref;
    }

    /// Get the next COUNT bits from the input buffer, saving incremental
    /// progress. Returns None on end of input, or if more than 15 bits are
    /// requested.
    fn get_bits(&mut self, count: u8) -> Option<u16> {
        if count > 15 {
            return None;
        }

        // If we aren't able to get COUNT bits, suspend immediately, because
        // we don't track how many bits of COUNT we've accumulated before
        // suspend.
        if self.input_size == 0 {
            if self.bit_index < (1 << count - 1) {
                return None;
            }
        }

        let mut accumulator: u16 = 0;
        let mut i: u8 = 0;

        while i < count {
            if self.bit_index == 0 {
                if self.input_size == 0 {
                    return None;
                }
                self.current_byte = self.input_buffer[self.input_index as usize];
                self.input_index += 1;
                if self.input_index == self.input_size {
                    // input_buffer is consumed
                    self.input_index = 0;
                    self.input_size = 0;
                }
                self.bit_index = 0x80;
            }
            accumulator = accumulator << 1;
            if self.current_byte & self.bit_index != 0 {
                accumulator |= 0x1;
            }
            self.bit_index = self.bit_index >> 1;
            i += 1;
        }

        return Some(accumulator);
    }

    /// Finish the uncompress stream
    pub fn finish(&self) -> HSfinishRes {
        // Return Done if input_buffer is consumed. Else return More.
        if self.input_size == 0 {
            HSfinishRes::HSRFinishDone
        } else {
            HSfinishRes::HSRFinishMore
        }
    }
}
