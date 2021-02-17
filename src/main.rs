use anyhow::{Context, Result};
use discord::model::Event;
use discord::{model::Message, Discord};
use dither::prelude::*;
use escposify::{img::Image as EscImage, printer::Printer};
use hyper::net::HttpsConnector;
use hyper::Client;
use hyper_native_tls::NativeTlsClient;
use image::GenericImageView;
use log::{error, info, LevelFilter};
use pos58_usb::POS58USB;
use std::env;
use std::io::Read;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const PRINTER_CHARS_PER_LINE: usize = 32;
const PRINTER_DOTS_PER_LINE: u32 = 384;

struct Handler {
    client: Client,
    ditherer: Ditherer<'static>,
    printer: Sender<PrinterMsg>,
}

enum PrinterMsg {
    Image(image::RgbImage),
    Text(String),
}

fn printer_thread(receiver: Receiver<PrinterMsg>) -> Result<()> {
    info!("Starting printer thread...");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut usb_context = libusb::Context::new().context("Failed to create LibUSB context.")?;
    let mut device = POS58USB::new(&mut usb_context, std::time::Duration::from_secs(1))
        .context("Failed to connect to printer")?;
    let mut printer = Printer::new(&mut device, None, None);
    printer
        .chain_align("ct")?
        .chain_println("Welcome to Discord!\n\n\n\n")?
        .flush()?;

    info!("Printer thread initialized!");
    while let Ok(msg) = receiver.recv() {
        match msg {
            PrinterMsg::Image(image) => {
                let image = EscImage::from(image::DynamicImage::ImageRgb8(image));
                printer
                    .chain_align("ct")?
                    .chain_bit_image(&image, None)?
                    .flush()?;
            }
            PrinterMsg::Text(text) => {
                printer.chain_align("lt")?.chain_println(&text)?.flush()?;
            }
        }
    }

    error!("Printer thread stopped.");

    Ok(())
}

impl Handler {
    pub fn new() -> Result<Self> {
        let ssl = NativeTlsClient::new()?;
        let connector = HttpsConnector::new(ssl);
        let client = hyper::Client::with_connector(connector);
        let ditherer = Ditherer::from_str("floyd")?;
        let (printer, receiver) = mpsc::channel();
        thread::spawn(move || log_result(printer_thread(receiver)));
        Ok(Self {
            client,
            ditherer,
            printer,
        })
    }

    pub fn handle(&mut self, message: Message) -> Result<()> {
        if !message.content.starts_with("!print") {
            return Ok(());
        }

        // Message header
        let author = message.author.name;
        let date = message.timestamp.format("%m/%d/%y %H:%M");
        let full_date = format!("{} {}:", author, date);
        let header = match full_date.chars().count() > PRINTER_CHARS_PER_LINE {
            true => format!("{}: ", author),
            false => full_date,
        };

        fatal_error(
            self.printer
                .send(PrinterMsg::Text(header))
                .context("Printer thread died"),
        );

        // Text printing
        let text = message.content.trim_start_matches("!print").trim_start();
        if !text.is_empty() {
            fatal_error(
                self.printer
                    .send(PrinterMsg::Text(text.into()))
                    .context("Printer thread died"),
            );
        }

        // Image printing
        for att in message.attachments {
            // Download the image
            let mut image = self
                .client
                .get(&att.url)
                .send()
                .context("Image download failed")?;

            // Read the image into local memory
            let mut buf = Vec::new();
            image.read_to_end(&mut buf).context("Image read failed")?;

            // Decode the image
            let image = image::load_from_memory(&buf).context("Image parse failed")?;

            // Resize to fit the printer
            let image = image.resize(
                PRINTER_DOTS_PER_LINE,
                9000,
                image::imageops::FilterType::Triangle,
            );

            // Convert to the ditherer's image format
            let image: Img<RGB<f64>> = Img::new(
                image.to_rgb8().pixels().map(|p| RGB::from(p.0)),
                image.width(),
            )
            .context("Image convert failed")?;

            // Dither the image
            let quantize = dither::create_quantize_n_bits_func(1)?;
            let image = image.convert_with(|rgb| rgb.to_chroma_corrected_black_and_white());
            let image = self
                .ditherer
                .dither(image, quantize)
                .convert_with(RGB::from_chroma_corrected_black_and_white);

            // Convert image back to normal...
            let (width, height) = image.size();
            let image = image::RgbImage::from_raw(width, height, image.raw_buf())
                .context("Could not convert back to a regular image")?;

            // Send image to the printer thread
            // NOTE: This is a fatal error; the expect is intentional!
            fatal_error(
                self.printer
                    .send(PrinterMsg::Image(image))
                    .context("Printer thread died"),
            );
        }
        Ok(())
    }
}

fn log_result(res: Result<()>) {
    if let Err(e) = res {
        error!("Error: {:#}", e);
    }
}

fn fatal_error(res: Result<()>) {
    if res.is_err() {
        log_result(res);
        panic!("Fatal error");
    }
}

fn main() -> Result<()> {
    // Set up logging
    let log_path = env::var("PRINTER_LOG_PATH").unwrap_or_else(|_| "print_bot.log".into());
    simple_logging::log_to_file(log_path, LevelFilter::Info)?;

    // Arguments
    let token = env::var("DISCORD_TOKEN")
        .context("Expected token in DISCORD_TOKEN environment variable.")?;

    // Set up printer concurrently with logging into Discord
    let handler = thread::spawn(|| Handler::new().map_err(|e| e.to_string()));

    // Log in to Discord using a bot token from the environment
    info!("Logging into discord");
    let discord = Discord::from_bot_token(&token).context("login failed")?;

    let mut handler = handler.join().unwrap().unwrap();

    // Establish and use a websocket connection
    let (mut connection, _) = discord.connect().context("connect failed")?;

    info!("Ready.");
    loop {
        match connection.recv_event() {
            Ok(Event::MessageCreate(message)) => {
                log_result(handler.handle(message));
            }
            Ok(_) => {}
            Err(discord::Error::Closed(code, body)) => {
                error!("Gateway closed on us with code {:?}: {}", code, body);
                break;
            }
            Err(err) => error!("Receive error: {:?}", err),
        }
    }
    Ok(())
}
