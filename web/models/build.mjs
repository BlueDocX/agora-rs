// Build the office's two model files from the Kenney packs (CC0, kenney.nl), unpacked
// in assets/kenney-src/ (see assets/kenney-src/README.md for where they come from):
//
//   people.glb     the characters, each a top-level node "cast_<file>", with only the
//                  animations the office plays, renamed "<character>/<clip>"
//   furniture.glb  the furniture pieces the office uses, each a node "kit_<file>"
//
// The prefixes keep the names unique: a piece's own inner nodes can share its file
// name (chairDesk has a node called "chair"), and the loader would rename one of them.
// (No ":" or "." in them: three.js strips those from node names.)
//
// Both get a gzipped copy, which the server sends to browsers that accept gzip.
//
//   cd web/models && npm ci --ignore-scripts && node build.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { Document, NodeIO } from "@gltf-transform/core";
import { dedup, getBounds, mergeDocuments, prune, unpartition } from "@gltf-transform/functions";
import { ALL_EXTENSIONS } from "@gltf-transform/extensions";

const SRC = "../../assets/kenney-src";
const OUT = "../../crates/agora-server/static/models";
// The characters use KHR_texture_transform for their colors: keep it.
const io = new NodeIO().registerExtensions(ALL_EXTENSIONS);

// Who plays whom is decided in office.js; these are the cast.
export const PEOPLE = [
  "character-male-d", // the Lead (suit and tie)
  "character-female-d", // the Reviewer (blazer)
  "character-female-b", "character-male-a", "character-female-e", "character-male-f",
  "character-female-c", "character-male-b", "character-female-f", "character-male-e",
];
const CLIPS = new Set(["idle", "walk", "sit", "interact-right", "emote-yes", "emote-no", "pick-up"]);

const FURNITURE = [
  "desk", "chairDesk", "computerScreen", "computerKeyboard", "computerMouse", "tableRound", "chair",
  "loungeSofa", "loungeChair", "tableCoffee", "rugRound", "rugRectangle", "pottedPlant", "plantSmall1",
  "plantSmall2", "bookcaseOpen", "books", "lampRoundFloor", "kitchenFridgeSmall", "kitchenCoffeeMachine",
  "sideTable", "wall", "wallWindow", "wallCorner", "floorFull", "laptop", "loungeDesignSofa", "tableCoffeeGlass",
];

/** Merge `files` into one document; each source's scene becomes one named node.
 *  With `center`, each piece is moved so its footprint is centered on the origin and
 *  it stands on y = 0 (Kenney pieces start at a corner), which makes placing easy. */
async function bundle(files, name, keepAnimation, center, prefix) {
  const out = new Document();
  out.createBuffer();
  const scene = out.createScene(name);
  for (const [id, path] of files) {
    const src = await io.read(path);
    for (const anim of src.getRoot().listAnimations()) {
      if (keepAnimation && keepAnimation(anim.getName())) anim.setName(`${id}/${anim.getName()}`);
      else anim.dispose();
    }
    const before = new Set(out.getRoot().listScenes());
    mergeDocuments(out, src);
    // The merged-in scene's top-level nodes go under one node named for the piece.
    const group = out.createNode(prefix + id);
    for (const s of out.getRoot().listScenes()) {
      if (before.has(s)) continue;
      for (const child of s.listChildren()) group.addChild(child);
      s.dispose();
    }
    if (center) {
      const { min, max } = getBounds(group);
      const offset = [-(min[0] + max[0]) / 2, -min[1], -(min[2] + max[2]) / 2];
      for (const child of group.listChildren()) {
        const t = child.getTranslation();
        child.setTranslation([t[0] + offset[0], t[1] + offset[1], t[2] + offset[2]]);
      }
    }
    scene.addChild(group);
  }
  out.getRoot().setDefaultScene(scene);
  await out.transform(dedup(), prune(), unpartition());
  const glb = await io.writeBinary(out);
  const gz = gzipSync(glb, { level: 9, mtime: 0 });
  writeFileSync(`${OUT}/${name}.glb`, glb);
  writeFileSync(`${OUT}/${name}.glb.gz`, gz);
  console.log(`${name}.glb ${glb.length} B, gzipped ${gz.length} B`);
}

// `node build.mjs people` (or `furniture`) builds just that one.
const only = process.argv[2];
if (!only || only === "people") await bundle(PEOPLE.map((id) => [id, `${SRC}/mini/Models/GLB format/${id}.glb`]), "people", (clip) => CLIPS.has(clip), false, "cast_");
if (!only || only === "furniture") await bundle(FURNITURE.map((id) => [id, `${SRC}/furniture/Models/GLTF format/${id}.glb`]), "furniture", null, true, "kit_");
// Sanity: every clip the office plays exists for every character.
const people = await io.readBinary(readFileSync(`${OUT}/people.glb`));
const names = new Set(people.getRoot().listAnimations().map((a) => a.getName()));
const missing = PEOPLE.flatMap((p) => [...CLIPS].map((c) => `${p}/${c}`)).filter((n) => !names.has(n));
if (missing.length) throw new Error("missing clips: " + missing.join(", "));
