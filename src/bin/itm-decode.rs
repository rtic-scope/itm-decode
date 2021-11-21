use anyhow::{Context, Result};
use itm_decode::{Decoder, DecoderOptions, TracePacket};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(
    about = "An ITM/DWT packet protocol decoder, as specified in the ARMv7-M architecture reference manual, Appendix D4. See <https://developer.arm.com/documentation/ddi0403/ed/>. Report bugs and request features at <https://github.com/tmplt/itm-decode>."
)]
struct Opt {
    #[structopt(
        short,
        long,
        help = "Presume decode errors are trivially resolved (effectivly assumes the next byte in the bitstream after anerror is a new header)"
    )]
    naive: bool,

    #[structopt(
        short = "-s",
        long = "--stimulus-strings",
        help = "Decode instumentation packets as UTF-8 strings (assumes each string ends with a newline)"
    )]
    instr_as_string: bool,

    #[structopt(
        name = "FILE",
        parse(from_os_str),
        help = "Raw trace input file. If \"-\" or omitted, expects raw trace on stdin instead."
    )]
    file: Option<PathBuf>,
}

fn main() -> Result<()> {
    let opt = Opt::from_args();

    // Open the given file, or stdin
    let mut file: Box<dyn BufRead> = match opt.file {
        Some(ref file) if file.to_str() != Some("-") => Box::new(BufReader::new(
            File::open(file.clone()).with_context(|| format!("Failed to open {:?}", file))?,
        )),
        _ => Box::new(BufReader::new(io::stdin())),
    };

    let mut decoder: Decoder<std::fs::File> = todo!(); //Decoder::new(DecoderOptions::default());
    let mut stim = if opt.instr_as_string {
        Some(BTreeMap::new())
    } else {
        None
    };

    loop {
        match decoder.next() {
            Ok(None) => {
                let mut buf = [0_u8; 8];
                if file
                    .read(&mut buf)
                    .with_context(|| "Unable to read input".to_string())?
                    == 0
                {
                    break; // EOF
                }
                decoder.push(&buf);
            }
            Ok(Some(TracePacket::Instrumentation { port, payload })) if opt.instr_as_string => {
                let stim = stim.as_mut().unwrap();
                // lossily convert payload to UTF-8 string
                stim.entry(port).or_insert_with(String::new);
                let string = stim.get_mut(&port).unwrap();
                string.push_str(&String::from_utf8_lossy(&payload));

                // If a newline is encountered, the user likely wants
                // the string to be printed.
                if let Some(c) = string.chars().last() {
                    if c == '\n' {
                        for line in string.lines() {
                            println!("port {}> {}", port, line);
                        }

                        string.clear();
                    }
                }
            }
            Ok(Some(packet)) => println!("{:?}", packet),

            Err(e) if !opt.naive => {
                println!("Error: {:?}", e);
                break;
            }
            Err(e) if opt.naive => {
                println!("Error: {:?}", e);
            }
            _ => unreachable!(),
        }
    }

    if let Some(stim) = stim {
        if stim.iter().any(|(_, string)| !string.is_empty()) {
            println!("Warning: decoded incomplete UTF-8 strings from instrumentation packets:");
        }
        for (port, string) in stim {
            for line in string.lines() {
                println!("port {}> {}", port, line);
            }
        }
    }

    Ok(())
}
