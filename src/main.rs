use anyhow::{Context, Result};
use discord::model::Event;
use discord::{model::Message, Discord};
use dither::prelude::*;
use escposify::{img::Image as EscImage, printer::Printer};
use hyper::client::IntoUrl;
use hyper::net::HttpsConnector;
use hyper::Client;
use hyper::Url;
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
const MAX_DOWNLOAD_SIZE: u64 = 1024 * 1024 * 8; // 8MB
//const MAX_DOWNLOAD_SIZE: u64 = 1024; // 8MB

/// Message handling service
struct Handler {
    client: Client,
    ditherer: Ditherer<'static>,
    printer: Sender<PrinterMsg>,
}

/// Message from discord thread to printer thread
enum PrinterMsg {
    Image(image::RgbImage),
    Text(String),
}

/// Printer thread is seperate from Discord thread to prevent blockage
fn printer_thread(receiver: Receiver<PrinterMsg>) -> Result<()> {
    info!("Starting printer thread...");

    // Device init
    let mut usb_context = libusb::Context::new().context("Failed to create LibUSB context.")?;
    let mut device = POS58USB::new(&mut usb_context, std::time::Duration::from_secs(1))
        .context("Failed to connect to printer")?;
    let mut printer = Printer::new(&mut device, None, None);

    // Welcome message
    printer
        .chain_align("ct")?
        .chain_println("Welcome to Discord!\n\n\n\n")?
        .flush()?;

    // Main print loop
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
    /// Create a new handler
    pub fn new() -> Result<Self> {
        // Hyper client
        let ssl = NativeTlsClient::new()?;
        let connector = HttpsConnector::new(ssl);
        let client = hyper::Client::with_connector(connector);

        // Channel for Discord <-> printer thread communication
        let (printer, receiver) = mpsc::channel();
        thread::spawn(move || log_result(printer_thread(receiver)));

        let ditherer = Ditherer::from_str("floyd")?;

        Ok(Self {
            client,
            ditherer,
            printer,
        })
    }

    /// Handle a received message
    pub fn handle(&mut self, message: Message) -> Result<()> {
        // Check if we are activated
        if !message.content.starts_with("!print") && !message.author.bot {
            return Ok(());
        }

        // Message header
        let author = message.author.name;
        let date = message.timestamp.format("%m/%d/%y %H:%M");
        info!(
            "Handling a new message from {}#{}",
            author, message.author.discriminator
        );
        let full_date = format!("{} {}:", author, date);
        let header = match full_date.chars().count() > PRINTER_CHARS_PER_LINE {
            true => format!("{}: ", author),
            false => full_date,
        };

        self.print_text(header);

        // Text printing
        let text = message.content.trim_start_matches("!print ");
        if !text.is_empty() {
            match validate_url(text) {
                Some(url) => self.print_image(url)?,
                None => self.print_text(text.into()),
            }
        }

        // Image printing
        for att in message.attachments {
            if att.dimensions().is_some() {
                if let Some(url) = validate_url(&att.url) {
                    self.print_image(url)?;
                }
            }
        }
        Ok(())
    }

    /// Print some text
    fn print_text(&self, text: String) {
        fatal_error(
            self.printer
                .send(PrinterMsg::Text(text))
                .context("Printer thread died"),
        );
    }

    /// Download and print some image
    fn print_image(&self, url: Url) -> Result<()> {
        // Download the image
        let image = self
            .client
            .get(url)
            .send()
            .context("Image download failed")?;

        // Read the image into local memory
        let mut buf = Vec::new();
        image
            .take(MAX_DOWNLOAD_SIZE)
            .read_to_end(&mut buf)
            .context("Image read failed")?;
        if buf.len() as u64 == MAX_DOWNLOAD_SIZE {
            error!("Attachment size reached maximum download size, {} bytes", MAX_DOWNLOAD_SIZE);
        }

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

        Ok(())
    }
}

/// Check if this is a valid image URL
fn validate_url(s: impl IntoUrl) -> Option<Url> {
    let url = s.into_url().ok()?;
    let file_name = url.path_segments()?.last()?;
    let file_extension = file_name.split('.').last()?;

    // https://discord.com/developers/docs/reference
    const VALID_EXTENSIONS: [&str; 5] = ["gif", "png", "webp", "jpeg", "jpg"];
    match VALID_EXTENSIONS.contains(&file_extension) {
        true => Some(url),
        false => None,
    }
}

/// Log a result as an error
fn log_result(res: Result<()>) {
    if let Err(e) = res {
        error!("Error: {:#}", e);
    }
}

/// Log a fatal error and panic
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
