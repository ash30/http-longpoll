# HTTP Longpoll 

- http long poll functionality to allow graceful degradation of web sockets in rust
- WIP ( as of 5th April 2025 )

## Crates

### `longpoll`

Provides server primitives to enable a long poll 'stream' and 'sink' api.

### `axum-eio`

Thin abstraction over ws to allow transparent degradation 
