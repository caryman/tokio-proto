//! Provides the substrate for implementing pipelined, streaming protocols.
//!
//! In most cases, it's sufficient to work with `streaming::pipeline::{Client,
//! Server}` instead. But for some advanced protocols in which the client and
//! servers have more of a peer relationship, it's useful to work directly with
//! these implementation details.

use futures::sync::mpsc;
use futures::{Future, Poll, Async, Stream, Sink, AsyncSink, StartSend};
use std::{fmt, io};
use crate::streaming::{Message, Body};
use super::{Frame, Transport};
use crate::buffer_one::BufferOne;

// TODO:
//
// - Wait for service readiness
// - Handle request body stream cancellation

/// Provides protocol pipelining functionality in a generic way over clients
/// and servers. Used internally by `pipeline::Client` and `pipeline::Server`.
pub struct Pipeline<T> where T: Dispatch {
    // True as long as the transport `T` hasn't completed.
    transport_open: bool,

    // True as long as the channel of incoming requests is open.
    request_sender_open: bool,

    // Glues the service with the pipeline task
    dispatch: BufferOne<DispatchSink<T>>,

    // The `Sender` for the current request body stream
    out_body: Option<BodySender<T::BodyOut, T::Error>>,

    // The response body stream
    in_body: Option<T::Stream>,

    // True when the transport is fully flushed
    is_flushed: bool,
}

/// Message used to communicate through the multiplex dispatch
pub type PipelineMessage<T, B, E> = Result<Message<T, B>, E>;

/// Dispatch messages from the transport to the service
pub trait Dispatch {
    /// Type of underlying I/O object
    type Io;

    /// Message written to transport
    type In;

    /// Body written to transport
    type BodyIn;

    /// Messages read from the transport
    type Out;

    /// Outbound body frame
    type BodyOut;

    /// Transport error
    type Error: From<io::Error>;

    /// Body stream written to transport
    type Stream: Stream<Item = Self::BodyIn, Error = Self::Error>;

    /// Transport type
    type Transport: Transport<Item = Frame<Self::Out, Self::BodyOut, Self::Error>,
                              SinkItem = Frame<Self::In, Self::BodyIn, Self::Error>>;

    /// Mutable reference to the transport
    fn transport(&mut self) -> &mut Self::Transport;

    /// Process an out message
    fn dispatch(&mut self, message: PipelineMessage<Self::Out, Body<Self::BodyOut, Self::Error>, Self::Error>) -> io::Result<()>;

    /// Poll the next completed message
    fn poll(&mut self) -> Poll<Option<PipelineMessage<Self::In, Self::Stream, Self::Error>>, io::Error>;

    /// RPC currently in flight
    /// TODO: Get rid of
    fn has_in_flight(&self) -> bool;
}

struct DispatchSink<T> {
    inner: T,
}

type BodySender<B, E> = BufferOne<mpsc::Sender<Result<B, E>>>;

impl<T> Pipeline<T> where T: Dispatch {
    /// Create a new pipeline `Pipeline` dispatcher with the given service and
    /// transport
    pub fn new(dispatch: T) -> Pipeline<T> {
        // Add `Sink` impl for `Dispatch`
        let dispatch = DispatchSink { inner: dispatch };

        // Add a single slot buffer for the sink
        let dispatch = BufferOne::new(dispatch);

        Pipeline {
            transport_open: true,
            request_sender_open: true,
            dispatch: dispatch,
            out_body: None,
            in_body: None,
            is_flushed: true,
        }
    }

    /// Returns true if the pipeline server dispatch has nothing left to do
    fn is_done(&self) -> bool {
        (!self.transport_open || !self.request_sender_open) && self.is_flushed && !self.has_in_flight()
    }

    fn read_out_frames(&mut self) -> io::Result<()> {
        while self.transport_open {
            // Return true if the pipeliner can process new outbound frames
            if !self.check_out_body_stream() {
                break;
            }

            if let Async::Ready(frame) = self.dispatch.get_mut().inner.transport().poll()? {
                self.process_out_frame(frame)?;
            } else {
                break;
            }
        }

        Ok(())
    }

    fn check_out_body_stream(&mut self) -> bool {
        let body = match self.out_body {
            Some(ref mut body) => body,
            None => return true,
        };

        body.poll_ready().is_ready()
    }

    fn process_out_frame(&mut self,
                         frame: Option<Frame<T::Out, T::BodyOut, T::Error>>)
                         -> io::Result<()> {
        trace!("process_out_frame");
        // At this point, the service & transport are ready to process the
        // frame, no matter what it is.
        match frame {
            Some(Frame::Message { message, body }) => {
                if body {
                    trace!("read out message with body");

                    let (tx, rx) = Body::pair();
                    let message = Message::WithBody(message, rx);

                    // Track the out body sender. If `self.out_body`
                    // currently holds a sender for the previous out body, it
                    // will get dropped. This terminates the stream.
                    self.out_body = Some(BufferOne::new(tx));

                    self.dispatch.get_mut().inner.dispatch(Ok(message))?;
                } else {
                    trace!("read out message");

                    let message = Message::WithoutBody(message);

                    // There is no streaming body. Set `out_body` to `None` so that
                    // the previous body stream is dropped.
                    self.out_body = None;

                    self.dispatch.get_mut().inner.dispatch(Ok(message))?;
                }
            }
            Some(Frame::Body { chunk }) => {
                match chunk {
                    Some(chunk) => {
                        trace!("read out body chunk");
                        self.process_out_body_chunk(chunk)?;
                    }
                    None => {
                        trace!("read out body EOF");
                        // Drop the sender.
                        // TODO: Ensure a sender exists
                        let _ = self.out_body.take();
                    }
                }
            }
            None => {
                trace!("read None");
                // At this point, we just return. This works
                // because tick() will be called again and go
                // through the read-cycle again.
                self.transport_open = false;
            }
            Some(Frame::Error { .. }) => {
                // At this point, the transport is toast, there
                // isn't much else that we can do. Killing the task
                // will cause all in-flight requests to abort, but
                // they can't be written to the transport anyway...
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "An error occurred."));
            }
        }

        Ok(())
    }

    fn process_out_body_chunk(&mut self, chunk: T::BodyOut) -> io::Result<()> {
        trace!("process_out_body_chunk");
        let mut reset = false;
        match self.out_body {
            Some(ref mut body) => {
                debug!("sending a chunk");

                // Try sending the out body chunk
                match body.start_send(Ok(chunk)) {
                    Ok(AsyncSink::Ready) => debug!("immediately done"),
                    Err(_e) => reset = true, // interest canceled
                    Ok(AsyncSink::NotReady(_)) => {
                        // poll_ready() is checked before entering this path
                        unreachable!();
                    }
                }
            }
            None => {
                debug!("interest canceled");
                // The rx half canceled interest, there is nothing else to do
            }
        }
        if reset {
            self.out_body = None;
        }
        Ok(())
    }

    fn write_in_frames(&mut self) -> io::Result<()> {
        trace!("write_in_frames");
        while self.dispatch.poll_ready().is_ready() {
            // Ensure the current in body is fully written
            if !self.write_in_body()? {
                debug!("write in body not done");
                break;
            }
            debug!("write in body done");

            // Write the next in-flight in message
            match self.dispatch.get_mut().inner.poll()? {
                Async::Ready(Some(Ok(message))) => {
                    trace!("   --> got message");
                    self.write_in_message(Ok(message))?;
                }
                Async::Ready(Some(Err(error))) => {
                    trace!("   --> got error");
                    self.write_in_message(Err(error))?;
                }
                Async::Ready(None) => {
                    trace!("   --> got None");
                    // The service is done with the connection.
                    self.request_sender_open = false;
                    break;
                }
                // Nothing to dispatch
                Async::NotReady => break,
            }
        }

        Ok(())
    }

    fn write_in_message(&mut self, message: Result<Message<T::In, T::Stream>, T::Error>) -> io::Result<()> {
        trace!("write_in_message");
        match message {
            Ok(Message::WithoutBody(val)) => {
                trace!("got in_flight value without body");
                let msg = Frame::Message { message: val, body: false };
                assert_send(&mut self.dispatch, msg)?;

                // TODO: don't panic maybe if this isn't true?
                assert!(self.in_body.is_none());

                // Track the response body
                self.in_body = None;
            }
            Ok(Message::WithBody(val, body)) => {
                trace!("got in_flight value with body");
                let msg = Frame::Message { message: val, body: true };
                assert_send(&mut self.dispatch, msg)?;

                // TODO: don't panic maybe if this isn't true?
                assert!(self.in_body.is_none());

                // Track the response body
                self.in_body = Some(body);
            }
            Err(e) => {
                trace!("got in_flight error");
                let msg = Frame::Error { error: e };
                assert_send(&mut self.dispatch, msg)?;
            }
        }

        Ok(())
    }

    // Returns true if the response body is fully written
    fn write_in_body(&mut self) -> io::Result<bool> {
        trace!("write_in_body");

        if self.in_body.is_some() {
            loop {
                // Even though this is checked before entering the function, checking should be
                // cheap and this is looped
                if !self.dispatch.poll_ready().is_ready() {
                    return Ok(false);
                }

                match self.in_body.as_mut().unwrap().poll() {
                    Ok(Async::Ready(Some(chunk))) => {
                        assert_send(&mut self.dispatch,
                                         Frame::Body { chunk: Some(chunk) })?;
                    }
                    Ok(Async::Ready(None)) => {
                        assert_send(&mut self.dispatch,
                                         Frame::Body { chunk: None })?;
                        break;
                    }
                    Err(_) => {
                        unimplemented!();
                    }
                    Ok(Async::NotReady) => {
                        debug!("not ready");
                        return Ok(false);
                    }
                }
            }
        }

        self.in_body = None;
        Ok(true)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.is_flushed = self.dispatch.poll_complete()?.is_ready();

        if let Some(ref mut out_body) = self.out_body {
            if out_body.poll_complete().is_ok() {
                return Ok(());
            }
        } else {
            return Ok(());
        }

        // Fall through and unset out_body
        self.out_body = None;
        Ok(())
    }

    fn has_in_flight(&self) -> bool {
        self.dispatch.get_ref().inner.has_in_flight()
    }
}

impl<T> Future for Pipeline<T> where T: Dispatch {
    type Item = ();
    type Error = io::Error;

    // Tick the pipeline state machine
    fn poll(&mut self) -> Poll<(), io::Error> {
        trace!("Pipeline::tick");

        // Always tick the transport first
        self.dispatch.get_mut().inner.transport().tick();

        // First read off data from the socket
        self.read_out_frames()?;

        // Handle completed responses
        self.write_in_frames()?;

        // Try flushing buffered writes
        self.flush()?;

        // Clean shutdown of the pipeline server can happen when
        //
        // 1. The server is done running, this is signaled by Transport::poll()
        //    returning None.
        //
        // 2. The transport is done writing all data to the socket, this is
        //    signaled by Transport::flush() returning Ok(Some(())).
        //
        // 3. There are no further responses to write to the transport.
        //
        // It is necessary to perfom these three checks in order to handle the
        // case where the client shuts down half the socket.
        //
        if self.is_done() {
            return Ok(().into())
        }

        // Tick again later
        Ok(Async::NotReady)
    }
}

impl<T> fmt::Debug for Pipeline<T>
    where T: Dispatch + fmt::Debug,
          T::In: fmt::Debug,
          T::BodyIn: fmt::Debug,
          T::BodyOut: fmt::Debug,
          T::Error: fmt::Debug,
          T::Stream: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pipeline")
            .field("transport_open", &self.transport_open)
            .field("request_sender_open", &self.request_sender_open)
            .field("dispatch", &self.dispatch)
            .field("out_body", &"Sender { ... }")
            .field("in_body", &self.in_body)
            .field("is_flushed", &self.is_flushed)
            .finish()
    }
}

impl<T: Dispatch> Sink for DispatchSink<T> {
    type SinkItem = <T::Transport as Sink>::SinkItem;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem)
                  -> StartSend<Self::SinkItem, io::Error>
    {
        self.inner.transport().start_send(item)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        self.inner.transport().poll_complete()
    }

    fn close(&mut self) -> Poll<(), io::Error> {
        self.inner.transport().close()
    }
}

impl<T: fmt::Debug> fmt::Debug for DispatchSink<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("DispatchSink")
            .field("inner", &self.inner)
            .finish()
    }
}

fn assert_send<S: Sink>(s: &mut S, item: S::SinkItem) -> Result<(), S::SinkError> {
    match s.start_send(item)? {
        AsyncSink::Ready => Ok(()),
        AsyncSink::NotReady(_) => {
            panic!("sink reported itself as ready after `poll_ready` but was \
                    then unable to accept a message")
        }
    }
}
