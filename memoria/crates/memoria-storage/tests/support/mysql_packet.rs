use tokio::io::{AsyncRead, AsyncReadExt};

/// Read one complete MySQL protocol packet without retaining its contents.
pub async fn packet(reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await?;
    let len = header[0] as usize | (header[1] as usize) << 8 | (header[2] as usize) << 16;
    let mut bytes = vec![0; len + 4];
    bytes[..4].copy_from_slice(&header);
    reader.read_exact(&mut bytes[4..]).await?;
    Ok(bytes)
}
