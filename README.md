# Tabletop Club Lobby Server

This repository contains the source code for [Tabletop Club](https://tabletopclub.net/)'s
lobby server, which is responsible for keeping track of multiplayer lobbies.

In essence, it is a WebSocket server that listens out for commands from clients
to either host or join rooms with unique four letter room codes, e.g. `ABCD`.

Once two clients have joined the same room, it is up to them to send each other
WebRTC messages (offers, answers, and candidates) so that they can establish
a direct peer-to-peer connection with each other.

## Compiling

To compile and run the server, you will need to install [Rust](https://www.rust-lang.org/tools/install)
for your platform.

### Building

```bash
git clone https://github.com/drwhut/tabletop_club_lobby_server.git
cd tabletop_club_lobby_server
cargo build --release
```

The resulting binary will be placed in: `target/release/ttc-lobby`.

### Running

```bash
cargo run --release
```

### Testing

```bash
cargo test
```

**NOTE:** Occasionally a test might fail with the following error:

```
error receiving message from server: Io(Os { code: 104, kind: ConnectionReset, message: "Connection reset by peer" })
```

As far as I can tell, this is dependent on how the host handles networking.
If you know of a way to stop this error and to make the tests 100% consistent,
please open an issue or pull request.

**macOS:** The test `connection::tests::accept_tls` is expected to fail due to
the test certificate having an expiry date of 10 years.

## Usage

Once you have the server running locally, how you configure Tabletop Club to
connect to your local instance, instead of the official global instance, depends
on which version of Tabletop Club you are using:

### Tabletop Club v0.1.x

1. Run the server with TLS encryption using the test certificate and private key.

   ```bash
   cargo run -- -p 9080 -t -x tests/certificate.crt -k tests/private.pem
   ```

2. Run the game by passing the following command line arguments:

   ```bash
   ./TabletopClub.x86_64 --master-server wss://localhost:9080 --ssl-certificate /full/path/to/certificate.crt
   ```

3. In the main menu, click "Multiplayer", then "Host Game". Your local server
   should create a room, and give your game instance the room code.

### Tabletop Club v0.2.x+

TODO

### Configuration

The server uses a configuration file to set specific values at runtime.
By default, the server will create one with the name `server.toml`, but this can
be changed using the `-c` argument.

With it, you can change properties such as the maximum number of rooms, the
maximum message size, or the ping interval.

*You do not need to restart the server to change the configuration.*
Instead, the server will check the configuration file at the same location every
30 seconds to see if it has been modified. If it has, then it automatically
updates the used values on the fly.

For example, if there are currently 10 rooms open, and you edit the file such
that `max_rooms = 5`, in the next 30 seconds the server will close five of the
rooms to adhere to the new maximum.

### Arguments

* `-c`, `--config`: Set the file path to the configuration file. This file is
  periodically checked for changes. If the file does not exist, one is
  automatically created. Default value is `server.toml`.

  ```bash
  cargo run -- -c my_config.toml
  ```

* `-p`, `--port`: Set the port number the server uses to listen for connections.
  Default is `9080`.

  ```bash
  cargo run -- -p 9550
  ```

* `-t`, `--tls`: Enables TLS encryption. If used, a certificate and private key
  must be provided.

  * `-x`, `--x509-certificate`: The file path to the X509 certificate used as
    the public key for TLS encryption.

  * `-k`, `--private-key`: The file path to the PEM-encoded private key.

  ```bash
  cargo run -- -t -x certificate.crt -k private.pem
  ```

## Reverse Proxy

The server does not provide security measures like rate or connection limiting
as is. Instead, if you wish to use this server in a production environment, then
it is *highly* recommended to use a reverse proxy in front of this server.

Here is an example configuration file for forwarding requests to the server
through Nginx:

```nginx
# Rate-limiting for establishing connections to the lobby server.
limit_req_zone $binary_remote_addr zone=lobby:10m rate=12r/m;

# Limit the number of connections from any given source.
limit_conn_zone $binary_remote_addr zone=remote:10m;

# Set the "Connection" header only if the "Upgrade" header is present.
map $http_upgrade $connection_upgrade {
	default	upgrade;
	''	close;
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;

    server_name lobby.mydomain.com;

    ssl_certificate /path/to/certificate.crt;
    ssl_certificate_key /path/to/private.pem;

    location / {
        proxy_pass http://localhost:9080;
        proxy_set_header Host $http_host;
        proxy_http_version 1.1;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Real-IP $remote_addr;

        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        # Only allow a connection from a given remote address once every five
        # seconds, with up to one extra.
        limit_req zone=lobby burst=1;

        # Only allow up to five total connections from a given remote address.
        limit_conn remote 5;
    }
}
```

**NOTE:** If you enable SSL at the reverse proxy stage, there is no need to
enable TLS on the server itself. That is, you can omit the `-t` argument.

## Metrics

The ability to monitor metrics, such as the number of open rooms or the number
of connected clients, will be added at a later date.
