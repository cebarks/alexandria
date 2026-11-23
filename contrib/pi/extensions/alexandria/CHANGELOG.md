# Changelog

## Unreleased

### Added

- `/alexandria` opens an auto-recall settings menu for the enabled state, result limit, and minimum similarity.

### Breaking

- The Pi companion reads `client.json` instead of `client.toml`. Existing TOML client files must be converted manually; `ALEXANDRIA_CLIENT_CONFIG` must point to a JSON file. The server's TOML configuration is unchanged.
