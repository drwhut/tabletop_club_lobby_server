# Contributing

## Bug Reports

If you come across a bug when using the server, please
[open an issue](https://github.com/drwhut/tabletop_club_lobby_server/issues),
or if you are confident you can fix it directly,
[open a pull request](https://github.com/drwhut/tabletop_club_lobby_server/pulls)
against the `master` branch.

If you decide to open a pull request, remember to run these commands *before*
pushing commits to ensure that the changes are valid:

```bash
cargo fmt
cargo test
```

## Feature Requests

This server is specifically made for Tabletop Club, and is not designed to be
a generic WebRTC signalling server. As such, features are only added when they
are necessary for Tabletop Club's multiplayer to function properly.

Therefore, it is unlikely that feature requests will be accepted.
