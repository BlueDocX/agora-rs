# Kenney asset packs (CC0)

The office's characters and furniture come from two packs by Kenney (www.kenney.nl),
released under CC0 (public domain). Credit is not required; we give it anyway.

| Pack | Page | Used for |
|---|---|---|
| Mini Characters 1.0 | https://kenney.nl/assets/mini-characters | the team, the Lead, the Reviewer (with their animations) |
| Furniture Kit 1.0 | https://kenney.nl/assets/furniture-kit | desks, chairs, computers, tables, lounge, plants, walls |

The packs themselves are not committed. To rebuild the models:

1. Download both zips from the pages above and unpack them here as `mini/` and `furniture/`.
2. `cd web/models && npm ci --ignore-scripts && node build.mjs` writes
   `crates/agora-server/static/models/people.glb` and `furniture.glb` (plus gzipped
   copies), keeping only what the office uses.
