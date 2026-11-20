use std::io::{self, Write};

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

pub fn write_response(mut w: impl Write, code: u16, headers: &[(&str, &str)], body: &[u8]) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.0 {code} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        status_text(code),
        body.len()
    )?;
    for (k, v) in headers {
        write!(w, "{k}: {v}\r\n")?;
    }
    w.write_all(b"\r\n")?;
    w.write_all(body)?;
    Ok(())
}

pub fn header_safe(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\0').take(180).collect()
}
