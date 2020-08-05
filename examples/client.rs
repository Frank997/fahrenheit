use fahrenheit::AsyncTcpStream;
use futures::io::{AsyncReadExt, AsyncWriteExt};

async fn http_get(addr: &str) -> Result<String, std::io::Error> {
    let mut conn = AsyncTcpStream::connect(addr)?;
    let _ = conn.write_all(b"GET / HTTP/1.0\r\n\r\n").await?;
    let mut page = Vec::new();
    loop {
        let mut buf = vec![0; 128];
        let len = conn.read(&mut buf).await?;  
        /* await = loop {
                        match future::poll(cx) {
                            Poll::Ready(x) => return x;  //如果是ready，返回调用await的函数的返回值
                            Poll::Pending => {} //否则pending，等待唤醒(reactor唤醒它还是excetor唤醒它？)
                        }
                    }
        */
        if len == 0 {
            break;
        }
        page.extend_from_slice(&buf[..len]);
    }
    let page = String::from_utf8_lossy(&page).into();
    Ok(page)
}

async fn get_google() {
    let res = http_get("google.com:80").await.unwrap();
    println!("{}", res);
}

fn main() {
    fahrenheit::run(get_google())
}
