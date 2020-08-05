use std::io;
use std::net::TcpListener;
use std::net::ToSocketAddrs;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures_core::Stream;

use crate::AsyncTcpStream;
use crate::REACTOR;

use log::debug;

// AsyncTcpListener just wraps std tcp listener
#[derive(Debug)]
pub struct AsyncTcpListener(TcpListener);

impl AsyncTcpListener {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> Result<AsyncTcpListener, io::Error> {
        let inner = TcpListener::bind(addr)?;

        inner.set_nonblocking(true)?;
        Ok(AsyncTcpListener(inner))
    }

    pub fn incoming(self) -> Incoming {
        Incoming(self.0)
    }
}

pub struct Incoming(TcpListener);

//Future 代表一个任务，Stream代表n个Future，可以通过poll_next来不断获取下一个任务
//Stream类似Future Iterator，会不断调用poll_next来获取下一个future(或者说future任务)，listener socket需要不断accept连接，因此将其抽象为Stream比较合适(不太确定，没试过，但直接用原生socket不断accept然后把每个返回的连接分别封装进不同的future里再传给reactor也行)
impl Stream for Incoming {
    type Item = AsyncTcpStream;

    fn poll_next(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        debug!("poll_next() called");

        let fd = self.0.as_raw_fd();
        let waker = ctx.waker();

        match self.0.accept() {  //阻塞直到有连接来
            Ok((conn, _)) => {
                let stream = AsyncTcpStream::from_std(conn).unwrap();
                Poll::Ready(Some(stream))  //返回stream
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {  //如果是EWOULDBLOCK，返回pending
                REACTOR.with(|reactor| reactor.add_read_interest(fd, waker.clone()));

                Poll::Pending
            }
            Err(err) => panic!("error {:?}", err),  //如果不是 阻塞error ，panic
        }
    }
}
