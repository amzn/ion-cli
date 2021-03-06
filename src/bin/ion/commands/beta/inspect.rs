use std::cell::RefCell;
use std::cmp::min;
use std::fmt::{Display, Write};
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::ops::Range;
use std::rc::Rc;
use std::str::{from_utf8_unchecked, FromStr};

use anyhow::{bail, Context, Result};
use clap::{App, Arg, ArgMatches};
use colored::Colorize;
use ion_rs::{BinaryIonCursor, IonType, Reader, SymbolTable, SystemEventHandler};
use ion_rs::result::IonResult;
use ion_rs::text::writer::TextWriter;
use memmap::MmapOptions;

const ABOUT: &str = "Displays hex-encoded binary Ion alongside its equivalent text for human-friendly debugging.";

// Creates a `clap` (Command Line Arguments Parser) configuration for the `inspect` command.
// This function is invoked by the `inspect` command's parent, `beta`, so it can describe its
// child commands.
pub fn app() -> App<'static, 'static> {
    App::new("inspect")
        .about(ABOUT)
        .arg(
            Arg::with_name("output")
                .long("output")
                .short("o")
                .takes_value(true)
                .help("Output file [default: STDOUT]"),
        )
        .arg(
            // Any number of input files can be specified by repeating the "-i" or "--input" flags.
            // Unlabeled positional arguments will also be considered input file names.
            Arg::with_name("input")
                .long("input")
                .short("i")
                .index(1)
                .multiple(true)
                .help("Input file"),
        )
        .arg(
            // This is named `skip-bytes` instead of `skip` to accommodate a future `skip-values` option.
            Arg::with_name("skip-bytes")
                .long("skip-bytes")
                .short("-s")
                .default_value("0")
                .hide_default_value(true)
                .help("Do not display any user values for the first `n` bytes of Ion data.")
                .long_help(
                    "When specified, the inspector will skip ahead `n` bytes before
beginning to display the contents of the stream. System values like
Ion version markers and symbol tables in the bytes being skipped will
still be displayed. If the requested number of bytes falls in the
middle of a value, the whole value (complete with field ID and
annotations if applicable) will be displayed. If the value is nested
in one or more containers, those containers will be displayed too."
                )
        )
        .arg(
            // This is named `limit-bytes` instead of `limit` to accommodate a future `limit-values` option.
            Arg::with_name("limit-bytes")
                .long("limit-bytes")
                .short("-l")
                .default_value("0")
                .hide_default_value(true)
                .help("Only display the next 'n' bytes of Ion data.")
                .long_help(
                    "When specified, the inspector will stop printing values after
processing `n` bytes of Ion data. If `n` falls within a value, the
complete value will be displayed."
                )
        )
}

// Create a type alias to simplify working with a shared, mutable reference to our output stream.
type OutputRef = Rc<RefCell<dyn io::Write>>;
// * The output stream could be STDOUT or a file handle, so we use `dyn io::Write` to abstract
//   over the two implementations.
// * The output stream will be shared by the IonInspector and the SystemEventHandler, so we use
//   an reference counting pointer (`Rc`) to allow each of them to own a reference to it.
// * Each entity that holds a reference to the output stream will need to mutate it, so we wrap it
//   in a `RefCell`, which adds a small amount of runtime cost to guarantee that only one owner
//   attempts to modify it at a time.
// * The Drop implementation will ensure that the output stream is flushed when the last reference
//   is dropped, so we don't need to do this manually.

// This function is invoked by the `inspect` command's parent, `beta`.
pub fn run(_command_name: &str, matches: &ArgMatches<'static>) -> Result<()> {
    // --skip-bytes has a default value, so we can unwrap this safely.
    let skip_bytes_arg = matches
        .value_of("skip-bytes")
        .unwrap();

    let bytes_to_skip = usize::from_str(skip_bytes_arg)
        // The `anyhow` crate allows us to augment a given Result with some arbitrary context that
        // will be displayed if it bubbles up to the end user.
        .with_context(|| format!("Invalid value for '--skip-bytes': '{}'", skip_bytes_arg))?;

    // --limit-bytes has a default value, so we can unwrap this safely.
    let limit_bytes_arg = matches
        .value_of("limit-bytes")
        .unwrap();

    let mut limit_bytes = usize::from_str(limit_bytes_arg)
        .with_context(|| format!("Invalid value for '--limit-bytes': '{}'", limit_bytes_arg))?;

    // If unset, --limit-bytes is effectively usize::MAX. However, it's easier on users if we let
    // them specify "0" on the command line to mean "no limit".
    if limit_bytes == 0 {
        limit_bytes = usize::MAX
    }

    let output: OutputRef;
    // If the user has specified an output file, use it.
    if let Some(file_name) = matches.value_of("output") {
        let output_file = File::create(file_name)
            .with_context(|| format!("Could not open '{}'", file_name))?;
        let buf_writer = BufWriter::new(output_file);
        output = Rc::new(RefCell::new(buf_writer));
    } else {
        // Otherwise, write to STDOUT.
        // TODO: Using io::stdout() isn't ideal as each write to stdout requires acquiring the
        //       STDOUT lock. Some research is required to see if there's a different handle we
        //       could use to avoid that. `io::stdout().lock()` won't work because io::stdout()
        //       (to which it refers) has a limited lifetime.
        let buf_writer = BufWriter::new(io::stdout());
        output = Rc::new(RefCell::new(buf_writer));
    }

    // Run the inspector on each input file that was specified.
    if let Some(input_file_iter) = matches.values_of("input") {
        for input_file_name in input_file_iter {
            let mut input_file = File::open(input_file_name)
                .with_context(|| format!("Could not open '{}'", input_file_name))?;
            inspect_file(input_file_name, &mut input_file, &output, bytes_to_skip, limit_bytes)?;
        }
    } else {
        // If no input file was specified, run the inspector on STDIN.

        // The inspector expects its input to be a byte array or mmap()ed file acting as a byte
        // array. If the user wishes to provide data on STDIN, we'll need to copy those bytes to
        // a temporary file and then read from that.

        // Create a temporary file that will delete itself when the program ends.
        let mut input_file = tempfile::tempfile()
            .with_context(|| concat!(
                "Failed to create a temporary file to store STDIN.",
                "Try passing an --input flag instead."
            ))?;

        // Pipe the data from STDIN to the temporary file.
        let mut writer = BufWriter::new(input_file);
        io::copy(&mut io::stdin(), &mut writer)
            .with_context(|| "Failed to copy STDIN to a temp file.")?;
        // Get our file handle back from the BufWriter
        input_file = writer.into_inner()
            .with_context(|| "Failed to read from temp file containing STDIN data.")?;
        // Read from the now-populated temporary file.
        inspect_file("STDIN temp file", &mut input_file, &output, bytes_to_skip, limit_bytes)?;
    }
    Ok(())
}

// Given a file, try to mmap() it and run the inspector over the resulting byte array.
fn inspect_file(input_file_name: &str,
                input_file: &mut File,
                output: &OutputRef,
                bytes_to_skip: usize,
                limit_bytes: usize) -> Result<()> {
    // mmap involves operating system interactions that inherently place its usage outside of Rust's
    // safety guarantees. If the file is unexpectedly truncated while it's being read, for example,
    // problems could arise.
    let mmap = unsafe {
        MmapOptions::new().map(&input_file)
            .with_context(|| format!("Could not mmap '{}'", input_file_name))?
    };

    // Treat the mmap as a byte array.
    let ion_data: &[u8] = &mmap[..];
    // Confirm that the input data is binary Ion, then run the inspector.
    match ion_data {
        // Pattern match the byte array to verify it starts with an IVM
        [0xE0, 0x01, 0x00, 0xEA, ..] => {
            let mut inspector = IonInspector::new(
                ion_data,
                Rc::clone(output),
                bytes_to_skip,
                limit_bytes,
            );

            write_header(&output)?;
            // This inspects all values at the top level, recursing as necessary.
            inspector.inspect_level()?;
        }
        _ => {
            // bail! constructs an `anyhow::Result` with the given context and returns.
            bail!("Input file '{}' does not appear to be binary Ion.", input_file_name);
        }
    };
    Ok(())
}

// The ion::Reader type allows you to specify an event handler to react to low-level events in the
// stream being read. This type summarizes them; it doesn't write out their full hex encoding,
// it just writes a comment describing the event in the text Ion column.
struct SystemLevelEventSummarizer {
    output: OutputRef,
    text_buffer: String,
}

impl SystemLevelEventSummarizer {
    pub fn new(output: OutputRef) -> SystemLevelEventSummarizer {
        SystemLevelEventSummarizer {
            output,
            text_buffer: String::with_capacity(512),
        }
    }
}

const IVM_HEX: &str = "e0 01 00 ea";
const IVM_TEXT: &str = "// Ion 1.0 Version Marker";
// System events (IVM, symtabs) are always at the top level.
const SYSTEM_EVENT_INDENTATION: &str = "";

impl SystemEventHandler for SystemLevelEventSummarizer {
    // TODO: At the moment, the SystemEventHandler trait's functions do not have a return type that
    //       would allow errors to bubble up. If writing to the output stream fails for some
    //       reason, the program will end and a more terse error message will be displayed.
    //       See: https://github.com/amzn/ion-rust/issues/118
    fn on_ivm(&mut self, _ion_version: (u8, u8)) {
        output(
            &self.output,
            None,
            None,
            SYSTEM_EVENT_INDENTATION,
            IVM_HEX,
            IVM_TEXT.dimmed(),
        ).expect("output() failure from on_ivm()");
    }

    fn on_symbol_table_append(&mut self, symbol_table: &SymbolTable, starting_id: usize) {
        self.text_buffer.clear();
        self.text_buffer.push_str("// Local symbol table append: [\"");
        join_into(&mut self.text_buffer, "\", \"", symbol_table.symbols_tail(starting_id).iter());
        self.text_buffer.push_str("\"]");
        output(
            &self.output,
            None,
            None,
            SYSTEM_EVENT_INDENTATION,
            "...",
            &self.text_buffer.dimmed(),
        ).expect("output() failure from on_symbol_table_append()");
    }

    fn on_symbol_table_reset(&mut self, symbol_table: &SymbolTable) {
        const ION_1_0_SYSTEM_TABLE_LENGTH: usize = 10;
        self.text_buffer.clear();
        if symbol_table.len() > ION_1_0_SYSTEM_TABLE_LENGTH {
            self.text_buffer.push_str("// New local symbol table: [\"");
            join_into(&mut self.text_buffer, "\", \"", symbol_table.symbols_tail(ION_1_0_SYSTEM_TABLE_LENGTH).iter());
            self.text_buffer.push_str("\"]");
        } else {
            self.text_buffer.push_str("// Using system symbol table");
        }

        output(
            &self.output,
            None,
            None,
            SYSTEM_EVENT_INDENTATION,
            "...",
            &self.text_buffer.dimmed(),
        ).expect("output() failure from on_symbol_table_reset()");
    }
}

const LEVEL_INDENTATION: &str = "  "; // 2 spaces per level
const TEXT_WRITER_INITIAL_BUFFER_SIZE: usize = 128;

struct IonInspector<'input> {
    output: OutputRef,
    reader: Reader<BinaryIonCursor<io::Cursor<&'input [u8]>>>,
    bytes_to_skip: usize,
    limit_bytes: usize,
    // Reusable buffer for formatting bytes as hex
    hex_buffer: String,
    // Reusable buffer for formatting text
    text_buffer: String,
    // Reusable buffer for colorizing text
    color_buffer: String,
    // Reusable buffer for tracking indentation
    indentation_buffer: String,
    // Text Ion writer for formatting scalar values
    text_ion_writer: TextWriter<Vec<u8>>,
}

impl<'input> IonInspector<'input> {
    fn new(input: &'input [u8], out: OutputRef, bytes_to_skip: usize, limit_bytes: usize) -> IonInspector<'input> {
        let mut reader = Reader::new(BinaryIonCursor::new(io::Cursor::new(input)));
        reader.set_symtab_event_handler(SystemLevelEventSummarizer::new(out.clone()));
        let text_ion_writer = TextWriter::new(Vec::with_capacity(TEXT_WRITER_INITIAL_BUFFER_SIZE));
        IonInspector {
            output: out,
            reader,
            bytes_to_skip,
            limit_bytes,
            hex_buffer: String::new(),
            text_buffer: String::new(),
            color_buffer: String::new(),
            indentation_buffer: String::new(),
            text_ion_writer,
        }
    }

    // Returns the offset of the first byte that pertains to the value on which the reader is
    // currently parked.
    fn first_value_byte_offset(&self) -> usize {
        if let Some(offset) = self.reader.field_id_offset() {
            return offset;
        }
        if let Some(offset) = self.reader.annotations_offset() {
            return offset;
        }
        self.reader.header_offset()
    }

    // Returns the byte offset range containing the current value and its annotations/field ID if
    // applicable.
    fn complete_value_range(&self) -> Range<usize> {
        let start = self.first_value_byte_offset();
        let end = self.reader.value_range().end;
        start..end
    }

    // Displays all of the values (however deeply nested) at the current level.
    fn inspect_level(&mut self) -> Result<()> {
        self.increase_indentation();

        // Per-level bytes skipped are tracked so we can add them to the text Ion comments that
        // appear each time some number of values is skipped.
        let mut bytes_skipped_this_level = 0;

        while let Some((ion_type, _is_null)) = self.reader.next()? {
            // See if we've already processed `bytes_to_skip` bytes; if not, move to the next value.
            let complete_value_range = self.complete_value_range();
            if complete_value_range.end <= self.bytes_to_skip {
                bytes_skipped_this_level += complete_value_range.len();
                continue;
            }

            // Saturating subtraction: if the result would underflow, the answer will be zero.
            let bytes_processed = complete_value_range.start.saturating_sub(self.bytes_to_skip);
            // See if we've already processed `limit_bytes`; if so, stop processing.
            if bytes_processed >= self.limit_bytes {
                let limit_message = if self.reader.depth() > 0 {
                    "// --limit-bytes reached, stepping out."
                } else {
                    "// --limit-bytes reached, ending."
                };
                output(
                    &self.output,
                    None,
                    None,
                    &self.indentation_buffer,
                    "...",
                    limit_message.dimmed(),
                )?;
                self.decrease_indentation();
                return Ok(());
            }

            // We're no longer skip-scanning to `bytes_to_skip`. If we skipped values at this depth
            // to get to this point, make a note of it in the output.
            if bytes_skipped_this_level > 0 {
                self.text_buffer.clear();
                write!(&mut self.text_buffer, "// Skipped {} bytes of user-level data", bytes_skipped_this_level)?;
                output(
                    &self.output,
                    None,
                    None,
                    &self.indentation_buffer,
                    "...",
                    &self.text_buffer.dimmed(),
                )?;
                bytes_skipped_this_level = 0;
            }

            self.write_field_if_present()?;
            self.write_annotations_if_present()?;
            // Print the value or, if it's a container, its opening delimiter: {, (, or [
            self.write_value()?;

            // If the current value is a container, step into it and inspect its contents.
            match ion_type {
                IonType::List | IonType::SExpression | IonType::Struct => {
                    self.reader.step_in()?;
                    self.inspect_level()?;
                    self.reader.step_out()?;
                    // Print the container's closing delimiter: }, ), or ]
                    output(
                        &self.output,
                        None,
                        None,
                        &self.indentation_buffer,
                        "",
                        &closing_delimiter_for(ion_type),
                    )?;
                }
                _ => {}
            }
        }

        self.decrease_indentation();
        Ok(())
    }

    fn increase_indentation(&mut self) {
        // Remove a level's worth of indentation from the buffer.
        if self.reader.depth() > 0 {
            self.indentation_buffer.push_str(LEVEL_INDENTATION);
        }
    }


    fn decrease_indentation(&mut self) {
        // Remove a level's worth of indentation from the buffer.
        if self.reader.depth() > 0 {
            let new_length = self.indentation_buffer.len() - LEVEL_INDENTATION.len();
            self.indentation_buffer.truncate(new_length);
        }
    }

    fn write_field_if_present(&mut self) -> IonResult<()> {
        if let Some(field_id) = self.reader.field_id() {
            self.hex_buffer.clear();
            to_hex(&mut self.hex_buffer, self.reader.raw_field_id_bytes().unwrap());

            let field_name = self.reader.field_name().expect("Field ID present, name missing.");
            self.text_buffer.clear();
            write!(&mut self.text_buffer, "'{}':", field_name)?;

            self.color_buffer.clear();
            write!(&mut self.color_buffer, " // ${}:", field_id)?;
            write!(&mut self.text_buffer, "{}", &self.color_buffer.dimmed())?;
            output(
                &self.output,
                self.reader.field_id_offset(),
                self.reader.field_id_length(),
                &self.indentation_buffer,
                &self.hex_buffer,
                &self.text_buffer,
            )?;
        }
        Ok(())
    }

    fn write_annotations_if_present(&mut self) -> IonResult<()> {
        let num_annotations = self.reader.annotation_ids().len();
        if num_annotations > 0 {
            self.hex_buffer.clear();
            to_hex(&mut self.hex_buffer, self.reader.raw_annotations_bytes().unwrap());

            self.text_buffer.clear();
            write!(&mut self.text_buffer, "'")?;
            join_into(&mut self.text_buffer, "'::'", self.reader.annotations());
            write!(&mut self.text_buffer, "'::")?;

            self.color_buffer.clear();
            write!(&mut self.color_buffer, " // $")?;
            join_into(&mut self.color_buffer, "::$", self.reader.annotation_ids().iter());
            write!(&mut self.color_buffer, "::")?;

            write!(self.text_buffer, "{}", self.color_buffer.dimmed())?;
            output(
                &self.output,
                self.reader.annotations_offset(),
                self.reader.annotations_length(),
                &self.indentation_buffer,
                &self.hex_buffer,
                &self.text_buffer,
            )?;
        }
        Ok(())
    }

    fn write_value(&mut self) -> IonResult<()> {
        self.text_buffer.clear();
        // Populates `self.text_buffer` with the Ion text representation of the current value
        // if it is a scalar. If the value is a container, format_value() will write the opening
        // delimiter of that container instead.
        self.format_value()?;

        self.hex_buffer.clear();
        to_hex(&mut self.hex_buffer, self.reader.raw_header_bytes().unwrap());
        // Only write the bytes representing the body of the value if it is a scalar.
        // If it is a container, `inspect_level` will handle stepping into it and writing any
        // nested values.
        if !self.reader.ion_type().unwrap().is_container() {
            self.hex_buffer.push_str(" ");
            to_hex(&mut self.hex_buffer, self.reader.raw_value_bytes().unwrap());
        }

        const TYPE_DESCRIPTOR_SIZE: usize = 1;
        let length = TYPE_DESCRIPTOR_SIZE + self.reader.header_length() + self.reader.value_length();
        output(
            &self.output,
            Some(self.reader.header_offset()),
            Some(length),
            &self.indentation_buffer,
            &self.hex_buffer,
            &self.text_buffer,
        )
    }

    fn format_value(&mut self) -> IonResult<()> {
        use ion_rs::IonType::*;

        // Destructure `self` to get multiple simultaneous mutable references to its constituent
        // fields. This freezes `self`; it cannot be referred to for the rest of the function call.
        let IonInspector {
            ref mut reader,
            ref mut text_ion_writer,
            ref mut text_buffer,
            ref mut color_buffer,
            ..
        } = self;

        // If we need to write comments alongside any of the values, we'll add them here so we can
        // colorize them separately.
        let comment_buffer = color_buffer;
        comment_buffer.clear();

        let writer = text_ion_writer; // Local alias for brevity.
        let ion_type = reader.ion_type().expect("format_value() called when reader was exhausted");
        if reader.is_null() {
            writer.write_null(reader.ion_type().unwrap())?;
        } else {
            match ion_type {
                Null => writer.write_null(ion_type),
                Boolean => writer.write_bool(reader.read_bool()?.unwrap()),
                Integer => writer.write_i64(reader.read_i64()?.unwrap()),
                Float => writer.write_f64(reader.read_f64()?.unwrap()),
                Decimal => writer.write_big_decimal(&reader.read_big_decimal()?.unwrap()),
                Timestamp => writer.write_datetime(&reader.read_datetime()?.unwrap()),
                Symbol => {
                    // TODO: Make this easier in the reader
                    let sid = reader.read_symbol_id()?.unwrap();
                    let text = reader
                        .symbol_table()
                        .text_for(sid)
                        .unwrap_or_else(|| panic!("Could not resolve text for symbol ID ${}", sid));
                    write!(comment_buffer, " // ${}", sid)?;
                    writer.write_symbol(text)
                }
                String => reader.string_ref_map(|s| writer.write_string(s))?.unwrap(),
                Clob => reader.clob_ref_map(|c| writer.write_clob(c))?.unwrap(),
                Blob => reader.blob_ref_map(|b| writer.write_blob(b))?.unwrap(),
                // The containers don't use the TextWriter to format anything. They simply write the
                // appropriate opening delimiter.
                List => {
                    write!(text_buffer, "[")?;
                    return Ok(());
                }
                SExpression => {
                    write!(text_buffer, "(")?;
                    return Ok(());
                }
                Struct => {
                    write!(text_buffer, "{{")?;
                    return Ok(());
                }
            }?;
        }
        // This is writing to a Vec, so flush() will always succeed.
        let _ = writer.flush();
        // The writer produces valid UTF-8, so there's no need to re-validate it.
        let value_text = unsafe { from_utf8_unchecked(writer.output().as_slice()) };
        write!(text_buffer, "{}", value_text.trim_end())?;
        // If we're in a container, add a delimiting comma. Text Ion allows trailing commas, so we
        // don't need to treat the last value as a special case.
        if self.reader.depth() > 0 {
            write!(text_buffer, ",")?;
        }
        write!(text_buffer, "{}", comment_buffer.dimmed())?;
        // Clear the writer's output Vec. We encode each scalar independently of one another.
        writer.output_mut().clear();
        Ok(())
    }
}

const COLUMN_DELIMITER: &str = " | ";
const CHARS_PER_HEX_BYTE: usize = 3;
const HEX_BYTES_PER_ROW: usize = 8;
const HEX_COLUMN_SIZE: usize = HEX_BYTES_PER_ROW * CHARS_PER_HEX_BYTE;

fn write_header(output: &OutputRef) -> IonResult<()> {
    // Unwrap our Rc<RefCell<dyn Write>> to get a &mut dyn Write for the rest of the function
    let mut output = output.borrow_mut();

    let line = "-".repeat(24 + 24 + 9 + 9 + (COLUMN_DELIMITER.len() * 3));

    writeln!(output, "{}", line)?;
    write!(output, "{:^9}{}", "Offset".bold().bright_white(), COLUMN_DELIMITER)?;
    write!(output, "{:^9}{}", "Length".bold().bright_white(), COLUMN_DELIMITER)?;
    write!(output, "{:^24}{}", "Binary Ion".bold().bright_white(), COLUMN_DELIMITER)?;
    writeln!(output, "{:^24}", "Text Ion".bold().bright_white())?;
    writeln!(output, "{}", line)?;
    Ok(())
}

// Accepting a `T` allows us to pass in `&str`, `&String`, `&ColoredString`, etc as out text_column
fn output<T: Display>(output: &OutputRef,
                      offset: Option<usize>,
                      length: Option<usize>,
                      indentation: &str,
                      hex_column: &str,
                      text_column: T) -> IonResult<()> {

    // Unwrap our Rc<RefCell<dyn Write>> to get a &mut dyn Write for the rest of the function
    let mut output = output.borrow_mut();

    // The current implementation always writes a single line of output for the offset, length,
    // and text columns. Only the hex column can span multiple rows.
    // TODO: It would be nice to allow important hex bytes (e.g. type descriptors or lengths)
    //       to be color-coded. This complicates the output function, however, as the length
    //       of a colored string is not the same as its display length. We would need to pass
    //       uncolored strings to the output function paired with the desired color/style so
    //       the output function could break the text into the necessary row lengths and then apply
    //       the provided colors just before writing.

    // Write the offset column
    if let Some(offset) = offset {
        write!(output, "{:9}{}", offset, COLUMN_DELIMITER)?;
    } else {
        write!(output, "{:9}{}", "", COLUMN_DELIMITER)?;
    }

    // Write the length column
    if let Some(length) = length {
        write!(output, "{:9}{}", length, COLUMN_DELIMITER)?;
    } else {
        write!(output, "{:9}{}", "", COLUMN_DELIMITER)?;
    }

    // If the hex string is short enough to fit in a single row...
    if hex_column.len() < HEX_COLUMN_SIZE {
        // ...print the hex string...
        write!(output, "{}", hex_column)?;
        // ...and then write enough padding spaces to fill the rest of the row.
        for _ in 0..(HEX_COLUMN_SIZE - hex_column.len()) {
            write!(output, " ")?;
        }
    } else {
        // Otherwise, write the first row's worth of the hex string.
        write!(output, "{}", &hex_column[..HEX_COLUMN_SIZE])?;
    }
    // Write a delimiter, the write the text Ion as the final column.
    write!(output, "{}", COLUMN_DELIMITER)?;
    write!(output, " ")?;
    writeln!(output, "{}{}", indentation, text_column)?;

    // Revisit our hex column. Write as many additional rows as needed.
    let mut col_1_written = HEX_COLUMN_SIZE;
    while col_1_written < hex_column.len() {
        // Padding for offset column
        write!(output, "{:9}{}", "", COLUMN_DELIMITER)?;
        // Padding for length column
        write!(output, "{:9}{}", "", COLUMN_DELIMITER)?;
        let remaining_bytes = &hex_column.len() - col_1_written;
        let bytes_to_write = min(remaining_bytes, HEX_COLUMN_SIZE);
        let next_slice_to_write = &hex_column[col_1_written..(col_1_written + bytes_to_write)];
        write!(output, "{}", next_slice_to_write)?;
        for _ in 0..(HEX_COLUMN_SIZE - bytes_to_write) {
            write!(output, " ")?;
        }
        writeln!(output, "{}", COLUMN_DELIMITER)?;
        col_1_written += HEX_COLUMN_SIZE;
        // No need to write anything for the text column since it's the last one.
    }
    Ok(())
}

fn closing_delimiter_for(container_type: IonType) -> &'static str {
    match container_type {
        IonType::List => "]",
        IonType::SExpression => ")",
        IonType::Struct => "}",
        _ => panic!("Attempted to close non-container type {:?}", container_type)
    }
}

fn to_hex(buffer: &mut String, bytes: &[u8]) {
    if bytes.len() == 0 {
        return;
    }
    write!(buffer, "{:02x}", bytes[0]).unwrap();
    for byte in &bytes[1..] {
        write!(buffer, " {:02x}", *byte).unwrap();
    }
}

fn join_into<T: Display>(buffer: &mut String,
                         delimiter: &str, mut values: impl Iterator<Item=T>) {
    if let Some(first) = values.next() {
        write!(buffer, "{}", first).unwrap();
    }
    while let Some(value) = values.next() {
        write!(buffer, "{}{}", delimiter, value).unwrap();
    }
}

