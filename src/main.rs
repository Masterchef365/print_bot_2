use anyhow::{format_err, Context, Result};
use chrono::prelude::*;
use discord::model::Event;
use discord::Discord;
use log::{error, info, LevelFilter};
use std::path::PathBuf;
use std::thread;
use structopt::StructOpt;

use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::Device;
use v4l::FourCC;

use std::sync::mpsc::{self, Receiver, Sender};

mod printer;
use printer::{PrintHandler, PrinterMsg};
type TimeRange = (NaiveTime, NaiveTime);

#[derive(Debug, StructOpt)]
#[structopt(name = "Printer bot 2", about = "A discord bot for receipt printers")]
struct Opt {
    /// Disable the camera
    #[structopt(long)]
    disable_camera: bool,

    /// Disable the printer
    #[structopt(long)]
    disable_printer: bool,

    /// Logging path
    #[structopt(long, default_value = "print_bot.log")]
    log_path: PathBuf,

    /// Discord token
    #[structopt(long)]
    token: String,

    /// Begin active hours (local, 24 hour)
    #[structopt(long)]
    begin_time: Option<String>,

    /// End active hours (local, 24 hour)
    #[structopt(long)]
    end_time: Option<String>,
}

// Settings
pub const HELP_COMMAND: &str = "!help";
pub const PRINT_COMMAND: &str = "!print";
pub const SHOW_COMMAND: &str = "!showme";
pub const LUA_COMMAND: &str = "!lua";

/// Log a result as an error
pub fn log_result(res: Result<()>) {
    if let Err(e) = res {
        error!("{:#}", e);
    }
}

/// Log a fatal error and panic
pub fn fatal_error(res: Result<()>) {
    if res.is_err() {
        log_result(res);
        panic!("Fatal error");
    }
}

fn parse_time(s: &str) -> Result<NaiveTime> {
    let mut s = s.split(':');
    match (s.next(), s.next()) {
        (Some(h), Some(m)) => Ok(NaiveTime::from_hms(h.parse()?, m.parse()?, 0)),
        (Some(_), None) => Err(format_err!("Time missing minutes")),
        (None, Some(_)) => unreachable!(),
        (None, None) => Err(format_err!("Malformed time")),
    }
}

fn lua_err(res: mlua::Error) -> anyhow::Error {
    format_err!("{}", res)
}

fn lua_thread(discord: Receiver<String>, printer: Option<Sender<PrinterMsg>>) -> Result<()> {
    info!("Lua thread started");
    let lua = mlua::Lua::new();

    fn print_res(printer: &Option<Sender<PrinterMsg>>, msg: String) -> Result<()> {
        match printer {
            Some(p) => Ok(p.send(PrinterMsg::Text(msg))?),
            None => Ok(eprintln!("No printer, LUA debug: {}", msg)),
        }
    }

    let lua_printer = printer.clone();
    let print = lua
        .create_function(move |_, v: String| Ok(print_res(&lua_printer, v).unwrap()))
        .map_err(lua_err)?;
    lua.globals().set("print", print).map_err(lua_err)?;

    loop {
        let msg = discord.recv()?;
        let msg = msg
            .trim_start()
            .trim_start_matches("```lua")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim_end();

        match lua.load(&msg).eval::<mlua::MultiValue>() {
            Err(e) => print_res(&printer, format!("Error: {}", e))?,
            Ok(v) => v
                .iter()
                .map(|v| print_res(&printer, value_to_string(v)))
                .collect::<Result<Vec<()>, _>>()
                .map(|_| ())?,
        }
    }
}

use mlua::Value;
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => "nil".into(),
        Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
        Value::Integer(i) => format!("{}", i),
        Value::Number(n) => format!("{}", n),
        Value::String(s) => format!("\"{}\"", s.to_str().unwrap_or("")),
        other => format!("{:?}", other),
    }
}

fn main() -> Result<()> {
    // Arg parsing
    let opt = Opt::from_args();
    let begin_time = opt.begin_time.as_ref().map(|s| parse_time(s)).transpose()?;
    let end_time = opt.end_time.as_ref().map(|s| parse_time(s)).transpose()?;
    let time_range = begin_time.zip(end_time);

    // Set up logging
    simple_logging::log_to_file(opt.log_path, LevelFilter::Info)?;

    // Set up printer concurrently with logging into Discord
    let print_handler = if opt.disable_printer {
        None
    } else {
        Some(thread::spawn(
            || -> Result<(PrintHandler, Sender<PrinterMsg>)> { Ok(PrintHandler::new()?) },
        ))
    };

    // Log in to Discord using a bot token from the environment
    info!("Logging into discord");
    let discord = Discord::from_bot_token(&opt.token).context("login failed")?;

    // Wait for the print handler...
    let mut print_handler = print_handler.and_then(|p| p.join().ok()).transpose()?;

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    // ################# CAMERA ######################

    // Create a new capture device with a few extra parameters
    let mut dev = (!opt.disable_camera)
        .then(|| -> Result<Device> {
            let dev = Device::new(0).context("Open device")?;

            // Let's say we want to explicitly request another format
            let mut fmt = dev.format().context("Read format")?;
            fmt.width = 1280;
            fmt.height = 720;
            fmt.fourcc = FourCC::new(b"MJPG");
            dev.set_format(&fmt).context("Write format")?;
            Ok(dev)
        })
        .transpose()
        .context("Failed to open camera")?;

    // Create the stream, which will internally 'allocate' (as in map) the
    // number of requested buffers for us.
    let mut stream = dev
        .as_mut()
        .map(|dev| -> Result<Stream> {
            let mut stream = Stream::with_buffers(dev, Type::VideoCapture, 4)
                .context("Failed to create buffer stream")?;

            // Prime the camera
            let steps = 5;
            for i in 1..=steps {
                info!("Priming the camera {}/{}", i, steps);
                stream.next()?;
            }

            Ok(stream)
        })
        .transpose()?;

    // ################# LUA ######################

    let (lua_tx, lua_rx) = mpsc::channel::<String>();
    let sender = print_handler.as_ref().map(|(_, s)| s.clone());
    let lua_thread = std::thread::spawn(move || lua_thread(lua_rx, sender));
    // TODO: Graceful shutdown command for lua channel?

    // ###############################################

    info!("Ready.");
    loop {
        match connection.recv_event() {
            Ok(Event::MessageCreate(message)) => {
                // No bots!
                if message.author.bot {
                    continue;
                }

                // Parse command from message
                let cmd = match message.content.split_whitespace().next() {
                    Some(cmd) => cmd,
                    None => continue,
                };

                // Run command
                match cmd {
                    PRINT_COMMAND => {
                        if let Some(range) = time_range {
                            if let Err(msg) = check_time(range) {
                                info!(
                                    "Rejecting print job from {} outside of time range",
                                    message.author.name
                                );
                                discord.send_message(message.channel_id, &msg, "", false)?;
                                continue;
                            }
                        }

                        info!(
                            "{}#{} began a print job.",
                            message.author.name, message.author.discriminator
                        );

                        if let Some((handler, _)) = &mut print_handler {
                            log_result(handler.handle_print_request(message));
                        } else {
                            discord.send_message(message.channel_id, SORRY_PRINTER, "", false)?;
                        }
                    }
                    LUA_COMMAND => {
                        lua_tx.send(message.content.trim_start_matches(LUA_COMMAND).to_string())?
                    }
                    HELP_COMMAND => {
                        discord.send_message(message.channel_id, HELP_TEXT, "", false)?;
                    }
                    SHOW_COMMAND => match stream
                        .as_mut()
                        .ok_or_else(|| format_err!("Camera not set up"))
                        .and_then(|s| Ok(s.next()?))
                    {
                        Ok((buf, _)) => {
                            info!(
                                "{}#{} took a picture.",
                                message.author.name, message.author.discriminator
                            );
                            discord
                                .send_file(message.channel_id, "", buf, "image.jpg")
                                .context("Failed to send image file!")?;
                        }
                        Err(e) => {
                            error!("Camera error: {}", e);
                            discord.send_message(message.channel_id, SORRY_CAMERA, "", false)?;
                        }
                    },
                    _ => (),
                }
            }
            Ok(_) => {}
            Err(discord::Error::Closed(code, body)) => {
                error!("Gateway closed on us with code {:?}: {}", code, body);
                break;
            }
            Err(err) => error!("Receive error: {:?}", err),
        }
    }

    lua_thread.join().unwrap()?;

    Ok(())
}

const HELP_TEXT: &str = "
**Segfault's printer bot**\n
This bot uses a receipt printer to immortalize your messages on 58mm thermal paper. Printer paper is extra super cheap, but remember that whatever you do print is waste.
If this command works, the printer _should_ be running. Have fun!

__Commands__:
`!print`: Print text or an image URL following this command, or attached images.
`!help`: Print this message
`!showme`: Take a picture of the printer, and show it here.
";

const SORRY_PRINTER: &str = "Sorry, the printer has been disabled for now :(";
const SORRY_CAMERA: &str = "Sorry, the camera has been disabled for now :(";

fn time_greater(a: NaiveTime, b: NaiveTime) -> bool {
    (a - b).num_milliseconds() > 0
}

fn time_test((begin, end): TimeRange, now: NaiveTime) -> bool {
    time_greater(end, begin) != (time_greater(now, end) == time_greater(now, begin))
}

fn check_time(range: TimeRange) -> Result<(), String> {
    let now = Local::now();
    let now_naive = now.naive_local().time();
    if time_test(range, now_naive) {
        Ok(())
    } else {
        let (begin, end) = range;
        Err(format!("Sorry, I'm asleep and the printer makes a bunch of noise. The bot is set up to become active between {} and {} (timezone: UTC{}). Please try again later!", begin, end, now.format("%:z")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time() {
        assert!(!time_test(
            (NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)),
            NaiveTime::from_hms(9, 0, 0),
        ));

        assert!(!time_test(
            (NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)),
            NaiveTime::from_hms(15, 0, 0),
        ));

        assert!(time_test(
            (NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)),
            NaiveTime::from_hms(13, 0, 0),
        ));

        assert!(time_test(
            (NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)),
            NaiveTime::from_hms(9, 0, 0),
        ));

        assert!(time_test(
            (NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)),
            NaiveTime::from_hms(15, 0, 0),
        ));

        assert!(!time_test(
            (NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)),
            NaiveTime::from_hms(13, 0, 0),
        ));
    }
}
