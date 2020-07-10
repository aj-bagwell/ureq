use crate::body::{copy_chunked, end_chunked};
use crate::error::Error;
use crate::response::set_stream;
use crate::response::Response;
use crate::stream::{DeadlineStream, Stream};
use crate::unit::{self, Unit};
use std::io::{Result as IoResult, Write};
use std::mem;

pub struct RequestWrite {
    unit: Unit,
    stream: DeadlineStream,
    body_empty: bool,
    connection_is_recycled: bool,
    finished: bool,
}

impl RequestWrite {
    pub(crate) fn new(unit: Unit) -> Result<Self, Error> {
        let (stream, connection_is_recycled) = unit::connect_and_send_prelude(&unit, true)?;
        let stream = DeadlineStream::new(stream, unit.deadline);
        Ok(RequestWrite {
            unit,
            stream,
            connection_is_recycled,
            body_empty: true,
            finished: false,
        })
    }

    // Returns true if this request, with the provided body, is retryable.
    pub(crate) fn is_retryable(&self) -> bool {
        // Per https://tools.ietf.org/html/rfc7231#section-8.1.3
        // these methods are idempotent.
        let idempotent = match self.unit.method.as_str() {
            "DELETE" | "GET" | "HEAD" | "OPTIONS" | "PUT" | "TRACE" => true,
            _ => false,
        };
        // Unsized bodies aren't retryable because we can't rewind the reader.
        // Sized bodies are retryable only if they are zero-length because of
        // coincidences of the current implementation - the function responsible
        // for retries doesn't have a way to replay a Payload.
        idempotent && self.body_empty
    }

    // This should only ever be called once either explicitly in finish() or when dropped
    fn do_finish(&mut self) -> Result<Response, Error> {
        assert!(!self.finished);
        self.finished = true;
        if self.unit.is_chunked {
            end_chunked(&mut self.stream)?;
        }
        // start reading the response to process cookies and redirects.
        let mut resp = Response::from_read(&mut self.stream);

        // https://tools.ietf.org/html/rfc7230#section-6.3.1
        // When an inbound connection is closed prematurely, a client MAY
        // open a new connection and automatically retransmit an aborted
        // sequence of requests if all of those requests have idempotent
        // methods.
        //
        // We choose to retry only once. To do that, we rely on is_recycled,
        // the "one connection per hostname" police of the ConnectionPool,
        // and the fact that connections with errors are dropped.
        //
        // TODO: is_bad_status_read is too narrow since it covers only the
        // first line. It's also allowable to retry requests that hit a
        // closed connection during the sending or receiving of headers.
        if let Some(err) = resp.synthetic_error() {
            if err.is_bad_status_read() && self.is_retryable() && self.connection_is_recycled {
                let (new_stream, is_recycled) = unit::connect_and_send_prelude(&self.unit, true)?;
                self.stream = DeadlineStream::new(new_stream, self.unit.deadline);
                self.connection_is_recycled = is_recycled;
                return self.do_finish();
            }
        }
        // squirrel away cookies
        unit::save_cookies(&self.unit, &resp);

        // handle redirects
        if resp.redirect() && self.unit.redirects > 0 {
            if self.unit.redirects == 1 {
                return Err(Error::TooManyRedirects);
            }

            // the location header
            let location = resp.header("location");
            if let Some(location) = location {
                // join location header to current url in case it it relative
                let new_url = self
                    .unit
                    .url
                    .join(location)
                    .map_err(|_| Error::BadUrl(format!("Bad redirection: {}", location)))?;

                // perform the redirect differently depending on 3xx code.
                match resp.status() {
                    301 | 302 | 303 => {
                        // recreate the unit to get a new hostname and cookies for the new host.
                        let new_unit = self.unit.redirect_to(new_url);

                        return Self::new(new_unit)?.do_finish();
                    }
                    _ => (),
                    // reinstate this with expect-100
                    // 307 | 308 | _ => connect(unit, method, use_pooled, redirects - 1, body),
                };
            }
        }

        set_stream(
            &mut resp,
            self.unit.url.to_string(),
            Some(self.unit.clone()),
            mem::replace(&mut self.stream, DeadlineStream::new(Stream::Empty, None)),
        );

        Ok(resp)
    }
    pub fn finish(mut self) -> Response {
        self.do_finish().unwrap_or_else(|e| e.into())
    }
}

impl Write for RequestWrite {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        if buf.len() > 0 {
            self.body_empty = false;
            if self.unit.is_chunked {
                let mut chunk = buf;
                copy_chunked(&mut chunk, &mut self.stream).map(|s| s as usize)
            } else {
                self.stream.write(buf)
            }
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> std::result::Result<(), std::io::Error> {
        self.stream.flush()
    }
}

impl Drop for RequestWrite {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.do_finish();
        }
    }
}

impl ::std::fmt::Debug for RequestWrite {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::result::Result<(), ::std::fmt::Error> {
        write!(f, "RequestWrite({} {})", self.unit.method, self.unit.url)
    }
}
