NEXT
----
- You can now pass a list of addresses to listen on
- When SIGTERM or SIGINT is received, shut down gracefully (unbind listening sockets but wait for in-flight sessions to end) for up to `shutdown-timeout-ms` milliseconds
- Add `-C` flag to check config and exit

1.0.2
-----
- `RSV` should always be set to 0x00
- Send response for unsupported commands after we finish reading the whole request
- Add some more metrics

1.0.1
-----
- Fix bug with unix domain stats sockets

1.0.0
-----
- Initial release
