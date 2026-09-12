//! PNG QR rendering shared by Bilibili / NetEase login.

use bytes::Bytes;
use feedbot_core::AuthError;
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use std::io::Cursor;

pub fn render_qr_png(content: &str) -> Result<Bytes, AuthError> {
    let code = QrCode::new(content.as_bytes()).map_err(|e| AuthError::Failed(e.to_string()))?;
    let image = code
        .render::<Luma<u8>>()
        .min_dimensions(512, 512)
        .quiet_zone(true)
        .build();
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|e| AuthError::Failed(e.to_string()))?;
    Ok(Bytes::from(output.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_magic_bytes() {
        let png = render_qr_png("https://example.com/qr").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() > 200);
    }
}
