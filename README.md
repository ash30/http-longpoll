# HTTP Longpoll
An implementation of HTTP Long Polling in Rust, for device accessiblity and/or graceful degradation of web sockets. The aim is to present a simple idiomatic stream/sink api, making handling long polling as convenient as working with WebSockets.

## Features
- Async Stream + Sink API
- Message batching and idle poll request timeouts
- Transparent integration with popular web frameworks

## Basic Example

```rust
use http_longpoll::{Session, Config};

let (handle, session) = Session::connect(Config::Default());

// simple echo
tokio::spawn(async move {
    let (mut tx, rx) = sesssion.split();
    while let Some(message) = rx.next().await {
        println!("Received: {:?}", message);
        tx.send(message).await;
    }
});
```

See [examples] folder for more detailed getting started details

## Crates

### `http-longpoll`

Core functionality implementing the long polling primitives with a Stream/Sink API.

### `http-longpoll-fallback (WIP)`

Thin abstraction over ws to allow transparent degradation to long poll.

## Status

This project is under active development. While the core functionality is implemented, API changes may occur.

[examples]: https://github.com/ash30/http-longpoll/tree/main/examples/
