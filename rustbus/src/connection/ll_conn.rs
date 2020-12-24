use super::Error;
use super::Result;
use super::Timeout;
use crate::auth;
use crate::message_builder::MarshalledMessage;
use crate::wire::marshal;
use crate::wire::unmarshal;
use crate::ByteOrder;

use std::time;

use std::os::unix::io::RawFd;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;

use nix::cmsg_space;
use nix::sys::socket::{
    self, connect, recvmsg, sendmsg, socket, ControlMessage, ControlMessageOwned, MsgFlags,
    SockAddr, UnixAddr,
};
use nix::sys::uio::IoVec;

/// A lowlevel abstraction over the raw unix socket
#[derive(Debug)]
pub struct SendConn {
    stream: UnixStream,

    byteorder: ByteOrder,
    msg_buf_out: Vec<u8>,

    serial_counter: u32,
}

pub struct RecvConn {
    stream: UnixStream,

    msg_buf_in: Vec<u8>,
    cmsgs_in: Vec<ControlMessageOwned>,
}

pub struct DuplexConn {
    pub send: SendConn,
    pub recv: RecvConn,
}

impl RecvConn {
    pub fn can_read_from_source(&self) -> nix::Result<bool> {
        let mut fdset = nix::sys::select::FdSet::new();
        let fd = self.stream.as_raw_fd();
        fdset.insert(fd);

        use nix::sys::time::TimeValLike;
        let mut zero_timeout = nix::sys::time::TimeVal::microseconds(0);

        nix::sys::select::select(None, Some(&mut fdset), None, None, Some(&mut zero_timeout))?;
        Ok(fdset.contains(fd))
    }

    /// Reads from the source once but takes care that the internal buffer only reaches at maximum max_buffer_size
    /// so we can process messages separatly and avoid leaking file descriptors to wrong messages
    fn refill_buffer(&mut self, max_buffer_size: usize, timeout: Timeout) -> Result<()> {
        let bytes_to_read = max_buffer_size - self.msg_buf_in.len();

        const BUFSIZE: usize = 512;
        let mut tmpbuf = [0u8; BUFSIZE];
        let iovec = IoVec::from_mut_slice(&mut tmpbuf[..usize::min(bytes_to_read, BUFSIZE)]);

        let mut cmsgspace = cmsg_space!([RawFd; 10]);
        let flags = MsgFlags::empty();

        let old_timeout = self.stream.read_timeout()?;
        match timeout {
            Timeout::Duration(d) => {
                self.stream.set_read_timeout(Some(d))?;
            }
            Timeout::Infinite => {
                self.stream.set_read_timeout(None)?;
            }
            Timeout::Nonblock => {
                self.stream.set_nonblocking(true)?;
            }
        }
        let msg = recvmsg(
            self.stream.as_raw_fd(),
            &[iovec],
            Some(&mut cmsgspace),
            flags,
        )
        .map_err(|e| match e.as_errno() {
            Some(nix::errno::Errno::EAGAIN) => Error::TimedOut,
            _ => Error::NixError(e),
        });

        self.stream.set_nonblocking(false)?;
        self.stream.set_read_timeout(old_timeout)?;

        let msg = msg?;

        self.msg_buf_in
            .extend(&mut tmpbuf[..msg.bytes].iter().copied());
        self.cmsgs_in.extend(msg.cmsgs());
        Ok(())
    }

    pub fn bytes_needed_for_current_message(&self) -> Result<usize> {
        if self.msg_buf_in.len() < 16 {
            return Ok(16);
        }
        let (_, header) = unmarshal::unmarshal_header(&self.msg_buf_in, 0)?;
        let (_, header_fields_len) = crate::wire::util::parse_u32(
            &self.msg_buf_in[unmarshal::HEADER_LEN..],
            header.byteorder,
        )?;
        let complete_header_size = unmarshal::HEADER_LEN + header_fields_len as usize + 4; // +4 because the length of the header fields does not count

        let padding_between_header_and_body = 8 - ((complete_header_size) % 8);
        let padding_between_header_and_body = if padding_between_header_and_body == 8 {
            0
        } else {
            padding_between_header_and_body
        };

        let bytes_needed = complete_header_size as usize
            + padding_between_header_and_body
            + header.body_len as usize;
        Ok(bytes_needed)
    }

    // Checks if the internal buffer currently holds a complete message
    pub fn buffer_contains_whole_message(&self) -> Result<bool> {
        if self.msg_buf_in.len() < 16 {
            return Ok(false);
        }
        let bytes_needed = self.bytes_needed_for_current_message();
        match bytes_needed {
            Err(e) => {
                if let Error::UnmarshalError(unmarshal::Error::NotEnoughBytes) = e {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
            Ok(bytes_needed) => Ok(self.msg_buf_in.len() >= bytes_needed),
        }
    }
    /// Blocks until a message has been read from the conn or the timeout has been reached
    pub fn read_whole_message(&mut self, timeout: Timeout) -> Result<()> {
        // This whole dance around reading exact amounts of bytes is necessary to read messages exactly at their bounds.
        // I think thats necessary so we can later add support for unixfd sending
        //calc timeout in reference to this point in time
        let start_time = time::Instant::now();

        while !self.buffer_contains_whole_message()? {
            self.refill_buffer(
                self.bytes_needed_for_current_message()?,
                super::calc_timeout_left(&start_time, timeout)?,
            )?;
        }
        Ok(())
    }

    /// Blocks until one read towards the message has been performed from the conn or the timeout has been reached
    pub fn read_once(&mut self, timeout: Timeout) -> Result<()> {
        self.refill_buffer(self.bytes_needed_for_current_message()?, timeout)?;
        Ok(())
    }

    /// Blocks until a message has been read from the conn or the timeout has been reached
    pub fn get_next_message(&mut self, timeout: Timeout) -> Result<MarshalledMessage> {
        self.read_whole_message(timeout)?;
        let (hdrbytes, header) = unmarshal::unmarshal_header(&self.msg_buf_in, 0)?;
        let (dynhdrbytes, dynheader) =
            unmarshal::unmarshal_dynamic_header(&header, &self.msg_buf_in, hdrbytes)?;

        let (bytes_used, mut msg) = unmarshal::unmarshal_next_message(
            &header,
            dynheader,
            &self.msg_buf_in,
            hdrbytes + dynhdrbytes,
        )?;

        if self.msg_buf_in.len() != bytes_used + hdrbytes + dynhdrbytes {
            return Err(Error::UnmarshalError(unmarshal::Error::NotAllBytesUsed));
        }
        self.msg_buf_in.clear();

        for cmsg in &self.cmsgs_in {
            match cmsg {
                ControlMessageOwned::ScmRights(fds) => {
                    msg.body
                        .raw_fds
                        .extend(fds.iter().map(|fd| crate::wire::UnixFd::new(*fd)));
                }
                _ => {
                    // TODO what to do?
                    eprintln!("Cmsg other than ScmRights: {:?}", cmsg);
                }
            }
        }
        self.cmsgs_in.clear();

        Ok(msg)
    }
}

impl SendConn {
    /// get the next new serial
    pub fn alloc_serial(&mut self) -> u32 {
        let serial = self.serial_counter;
        self.serial_counter += 1;
        serial
    }

    /// send a message over the conn
    pub fn send_message<'a>(
        &'a mut self,
        msg: &'a mut MarshalledMessage,
    ) -> Result<SendMessageContext<'a>> {
        self.msg_buf_out.clear();
        let remove_later = if msg.dynheader.serial.is_some() {
            false
        } else {
            let serial = self.serial_counter;
            self.serial_counter += 1;
            msg.dynheader.serial = Some(serial);
            true
        };

        marshal::marshal(&msg, self.byteorder, &mut self.msg_buf_out)?;

        let ctx = SendMessageContext {
            msg,
            conn: self,

            progress: SendMessageProgress::default(),
            remove_serial_after_sending: remove_later,
        };

        Ok(ctx)
    }
}

/// only call if you deem the connection doomed by an error returned from writing.
/// The connection might be left in an invalid state if some but not all bytes of the message
/// have been written
pub fn force_finish_on_error<E>((s, e): (SendMessageContext<'_>, E)) -> E {
    s.force_finish();
    e
}

#[must_use = "Dropping this type is considered an error since it might leave the connection in an illdefined state if only some bytes of a message have been written"]
#[derive(Debug)]
/// Handles the process of actually sending a message over the connection it was created from. This allows graceful handling of short writes or timeouts with only
/// parts of the message written. You can loop over write or write_once or use write_all to wait until all bytes have been written or an error besides a timeout
/// arises.
pub struct SendMessageContext<'a> {
    msg: &'a mut MarshalledMessage,
    conn: &'a mut SendConn,

    remove_serial_after_sending: bool,

    progress: SendMessageProgress,
}

/// Tracks the progress of sending the message. Can be used to resume a SendMessageContext.
///
///Note that this only makes sense if you resume with the same Message and Connection that were used to create the original SendMessageContext.
#[derive(Debug, Default, Copy, Clone)]
pub struct SendMessageProgress {
    bytes_sent: usize,
}

/// This panics if the SendMessageContext was dropped when it was not yet finished. Use force_finish / force_finish_on_error
/// if you want to do this. It will be necessary for handling errors that make the connection unusable.
impl Drop for SendMessageContext<'_> {
    fn drop(&mut self) {
        if self.progress.bytes_sent != 0 && !self.all_bytes_written() {
            panic!("You dropped a SendMessageContext that only partially sent the message! This is not ok since that leaves the connection in an ill defined state. Use one of the consuming functions!");
        } else {
            if self.remove_serial_after_sending {
                self.msg.dynheader.serial = None;
            }
        }
    }
}

impl SendMessageContext<'_> {
    /// Resume a SendMessageContext from the progress. This needs to be called with the same
    /// conn and msg that were used to create the original SendMessageContext.
    pub fn resume<'a>(
        conn: &'a mut SendConn,
        msg: &'a mut MarshalledMessage,
        remove_serial_after_sending: bool,
        progress: SendMessageProgress,
    ) -> SendMessageContext<'a> {
        SendMessageContext {
            conn,
            msg,
            progress,
            remove_serial_after_sending,
        }
    }

    /// Turn this into the progress to resume the sending later. Note that you cannot send another
    /// message while doing that. You need to resume a SendMessageContext from this progress and
    /// send the current message beofre starting the next one.
    pub fn into_progress(self) -> SendMessageProgress {
        let progress = self.progress;
        Self::force_finish(self);
        progress
    }

    fn finish_if_ok<O, E>(
        self,
        res: std::result::Result<O, E>,
    ) -> std::result::Result<O, (Self, E)> {
        match res {
            Ok(o) => {
                // this is technically unnecessary but just to make it explicit we drop self here
                std::mem::drop(self);
                Ok(o)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// only call if you deem the connection doomed by an error returned from writing.
    /// The connection might be left in an invalid state if some but not all bytes of the message
    /// have been written
    pub fn force_finish(self) {
        std::mem::forget(self)
    }

    pub fn write(mut self, timeout: Timeout) -> std::result::Result<u32, (Self, super::Error)> {
        let start_time = std::time::Instant::now();

        // loop until either the time is up or all bytes have been written
        let res = loop {
            let iteration_timeout = super::calc_timeout_left(&start_time, timeout);
            let iteration_timeout = match iteration_timeout {
                Err(e) => break Err(e),
                Ok(t) => t,
            };

            match self.write_once(iteration_timeout) {
                Err(e) => break Err(e),
                Ok(t) => t,
            };
            if self.all_bytes_written() {
                break Ok(self.msg.dynheader.serial.unwrap());
            }
        };

        // This only occurs if all bytes have been sent. Otherwise we return with Error::TimedOut or another error
        self.finish_if_ok(res)
    }

    pub fn write_all(self) -> std::result::Result<u32, (Self, super::Error)> {
        self.write(Timeout::Infinite)
    }

    pub fn all_bytes_written(&self) -> bool {
        self.progress.bytes_sent == self.conn.msg_buf_out.len()
    }

    pub fn write_once(&mut self, timeout: Timeout) -> Result<usize> {
        let slice_to_send = &self.conn.msg_buf_out[self.progress.bytes_sent..];
        let iov = [IoVec::from_slice(slice_to_send)];
        let flags = MsgFlags::empty();

        let old_timeout = self.conn.stream.write_timeout()?;
        match timeout {
            Timeout::Duration(d) => {
                self.conn.stream.set_write_timeout(Some(d))?;
            }
            Timeout::Infinite => {
                self.conn.stream.set_write_timeout(None)?;
            }
            Timeout::Nonblock => {
                self.conn.stream.set_nonblocking(true)?;
            }
        }

        // if this is not the first write for this message do not send the raw_fds again. This would lead to unexpected
        // duplicated FDs on the other end!
        let raw_fds = if self.progress.bytes_sent == 0 {
            self.msg
                .body
                .raw_fds
                .iter()
                .map(|fd| fd.get_raw_fd())
                .flatten()
                .collect::<Vec<RawFd>>()
        } else {
            vec![]
        };
        let bytes_sent = sendmsg(
            self.conn.stream.as_raw_fd(),
            &iov,
            &[ControlMessage::ScmRights(&raw_fds)],
            flags,
            None,
        );

        self.conn.stream.set_write_timeout(old_timeout)?;
        self.conn.stream.set_nonblocking(false)?;

        let bytes_sent = bytes_sent?;

        self.progress.bytes_sent += bytes_sent;

        Ok(bytes_sent)
    }
}

impl DuplexConn {
    /// Connect to a unix socket and choose a byteorder
    pub fn connect_to_bus_with_byteorder(
        addr: UnixAddr,
        byteorder: ByteOrder,
        with_unix_fd: bool,
    ) -> super::Result<DuplexConn> {
        let sock = socket(
            socket::AddressFamily::Unix,
            socket::SockType::Stream,
            socket::SockFlag::empty(),
            None,
        )?;
        let sock_addr = SockAddr::Unix(addr);
        connect(sock, &sock_addr)?;
        let mut stream = unsafe { UnixStream::from_raw_fd(sock) };
        match auth::do_auth(&mut stream)? {
            auth::AuthResult::Ok => {}
            auth::AuthResult::Rejected => return Err(Error::AuthFailed),
        }

        if with_unix_fd {
            match auth::negotiate_unix_fds(&mut stream)? {
                auth::AuthResult::Ok => {}
                auth::AuthResult::Rejected => return Err(Error::UnixFdNegotiationFailed),
            }
        }

        auth::send_begin(&mut stream)?;

        Ok(DuplexConn {
            send: SendConn {
                stream: stream.try_clone()?,
                msg_buf_out: Vec::new(),
                byteorder,
                serial_counter: 1,
            },
            recv: RecvConn {
                msg_buf_in: Vec::new(),
                cmsgs_in: Vec::new(),
                stream,
            },
        })
    }

    /// Connect to a unix socket. The default little endian byteorder is used
    pub fn connect_to_bus(addr: UnixAddr, with_unix_fd: bool) -> Result<DuplexConn> {
        Self::connect_to_bus_with_byteorder(addr, ByteOrder::LittleEndian, with_unix_fd)
    }

    /// Sends the obligatory hello message and returns the unique id the daemon assigned this connection
    pub fn send_hello<'a>(
        &'a mut self,
        timeout: crate::connection::Timeout,
    ) -> super::Result<String> {
        let start_time = time::Instant::now();

        let mut hello = crate::standard_messages::hello();
        let serial = self
            .send
            .send_message(&mut hello)?
            .write(super::calc_timeout_left(&start_time, timeout)?)
            .map_err(|(ctx, e)| {
                ctx.force_finish();
                e
            })?;
        let resp = self
            .recv
            .get_next_message(super::calc_timeout_left(&start_time, timeout)?)?;
        if resp.dynheader.response_serial != Some(serial) {
            return Err(super::Error::AuthFailed);
        }
        let unique_name = resp.body.parser().get::<String>()?;
        Ok(unique_name)
    }
}

impl AsRawFd for SendConn {
    /// Reading or writing to the `RawFd` may result in undefined behavior
    /// and break the `Conn`.
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}
impl AsRawFd for RecvConn {
    /// Reading or writing to the `RawFd` may result in undefined behavior
    /// and break the `Conn`.
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}
impl AsRawFd for DuplexConn {
    /// Reading or writing to the `RawFd` may result in undefined behavior
    /// and break the `Conn`.
    fn as_raw_fd(&self) -> RawFd {
        self.recv.stream.as_raw_fd()
    }
}
