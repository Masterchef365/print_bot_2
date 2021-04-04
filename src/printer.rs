use anyhow::{Context, Result, anyhow};
use discord::model::Message;
use dither::prelude::*;
use escposify::{img::Image as EscImage, printer::Printer};
use hyper::client::IntoUrl;
use hyper::net::HttpsConnector;
use hyper::Client;
use hyper::Url;
use hyper_native_tls::NativeTlsClient;
use image::GenericImageView;
use log::{error, info};
use pos58_usb::POS58USB;
use std::io::Read;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const PRINTER_WELCOME: &str = "Welcome to Discord!\n\n\n\n";

const MAX_DOWNLOAD_SIZE: u64 = 1024 * 1024 * 8; // 8MB
pub const PRINTER_CHARS_PER_LINE: usize = 32;
pub const PRINTER_DOTS_PER_LINE: u32 = 384;

/// Message handling service
pub struct PrintHandler {
    client: Client,
    ditherer: Ditherer<'static>,
    printer: Sender<PrinterMsg>,
}

/// Message from discord thread to printer thread
pub enum PrinterMsg {
    Image(image::RgbImage),
    Text(String),
}

/// Printer thread is seperate from Discord thread to prevent blockage
fn printer_thread(receiver: &mut Receiver<PrinterMsg>) -> Result<()> {
    info!("Starting printer thread...");

    // Device init
    let mut usb_context = libusb::Context::new().context("Failed to create LibUSB context.")?;
    let mut device = POS58USB::new(&mut usb_context, std::time::Duration::from_secs(1))
        .context("Failed to connect to printer")?;
    let mut printer = Printer::new(&mut device, None, None);

    // Welcome message
    printer
        .chain_align("ct")?
        .chain_println(PRINTER_WELCOME)?
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

    Err(anyhow!("Printer thread stopped, restarting."))
}

impl PrintHandler {
    /// Create a new handler
    pub fn new() -> Result<(Self, Sender<PrinterMsg>)> {
        // Hyper client
        let ssl = NativeTlsClient::new()?;
        let connector = HttpsConnector::new(ssl);
        let client = hyper::Client::with_connector(connector);

        // Channel for Discord <-> printer thread communication
        let (printer, mut receiver) = mpsc::channel();
        thread::spawn(move || loop {
            crate::log_result(printer_thread(&mut receiver))
        });

        let ditherer = Ditherer::from_str("floyd")?;

        let sender = printer.clone();

        Ok((Self {
            client,
            ditherer,
            printer,
        }, sender))
    }

    /// Handle a printing command
    pub fn handle_print_request(&mut self, message: Message) -> Result<()> {
        // Check to see if there's anything to do
        let text = message
            .content
            .trim_start_matches(crate::PRINT_COMMAND)
            .trim_start();
        if text.is_empty() && message.attachments.is_empty() {
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

        // Message body printing
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
        crate::fatal_error(
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
            error!(
                "Attachment size reached maximum download size, {} bytes",
                MAX_DOWNLOAD_SIZE
            );
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
        crate::fatal_error(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_validation() {
        assert_eq!(validate_url("https://fuck.com"), None);
        assert_eq!(
            validate_url("https://fuck.com/wat.png"),
            Some(Url::parse("https://fuck.com/wat.png").unwrap())
        );
        assert_eq!(validate_url("https://fuck.com/wat.html"), None);
        assert_eq!(validate_url("wat.png"), None);
    }
}
