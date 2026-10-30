// The 3D office: a small diorama of a studio where the run's team works. Each agent
// (or each task, without a team) has a desk in the pod; they stand in the aisle while
// their work waits, walk to their chair and sit to work, and stand up to cheer when
// the work is accepted. The Lead sits at the meeting table; the reviewer walks the
// aisles to whoever has a draft.
//
// It only listens: the page dispatches `agora:event` (one run event), `agora:reset`
// (a run is about to replay), `agora:team` (the team picked while no run is open)
// `agora:chat` (chat traffic, shown as speech bubbles), `agora:errand` (an Alibi
// errand: the agent fetches records or works the laptop for each call; a red ring
// and the reviewer when the guard stops a draft, a green one when it is delivered)
// and `agora:bots` (each bot's latest errand: its desk monitor glows green for
// delivered with nothing false, red for corrected or withheld, amber while running).
// It says one thing back: `agora:pick` {id, name} when a desk or its person is clicked.
//
// Art: Kenney's Mini Characters and Furniture Kit (CC0, kenney.nl), trimmed into two
// files by web/models/build.mjs. Without WebGL2 the page keeps its kanban.

import {
  ACESFilmicToneMapping, AnimationMixer, BoxGeometry, CanvasTexture, DirectionalLight, Group, HemisphereLight,
  LoopOnce, LoopRepeat, Mesh, MeshBasicMaterial, MeshStandardMaterial, PCFSoftShadowMap, PerspectiveCamera, PlaneGeometry, PMREMGenerator,
  Raycaster, RingGeometry, Scene, SRGBColorSpace, Sprite, SpriteMaterial, Vector2, Vector3, WebGLRenderer,
} from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { RoomEnvironment } from "three/addons/environments/RoomEnvironment.js";
import { clone as cloneSkinned } from "three/addons/utils/SkeletonUtils.js";
import { VRButton } from "three/addons/webxr/VRButton.js";

const PALETTE = window.agoraPalette ?? ["#4285f4", "#34a853", "#ea4335", "#14b8a6", "#f97316", "#ec4899", "#64748b", "#a16207"];
const STATE = {
  idle: ["#b3aa8f", "scheduled"], working: ["#4285f4", "working"], review: ["#d9af1b", "in review"],
  accepted: ["#1f9d55", "done"], failed: ["#e5483b", "needs a person"], skipped: ["#b3aa8f", "skipped"],
  free: ["#b3aa8f", "available"], // an agent with no task on this run
};
const RANK = ["review", "working", "failed", "idle", "accepted", "skipped"];
// A bot's latest errand, as its desk shows it (the page's `agora:bots`).
const VERDICT = { ok: ["#1f9d55", "delivered"], bad: ["#e5483b", "corrected"], running: ["#f2a81d", "on an errand"] };
const SEATED = new Set(["working", "review"]);
// The cast (see web/models/build.mjs): the Lead, the Reviewer, then the team in order.
const LEAD = "character-male-d", REVIEWER = "character-female-d";
const TEAM = ["character-female-b", "character-male-a", "character-female-e", "character-male-f",
  "character-female-c", "character-male-b", "character-female-f", "character-male-e"];
const WALK_SPEED = 1.1; // units per second (a character is 0.72 tall)
const FLOOR = 0.05; // the floor's top
const SEAT_Y = 0.06; // how high a seated character sits above the floor
const ROOM = { x0: -3.5, x1: 3.5, z0: -3, z1: 2.6 };
const POD_X = 0.85, DESK_GAP = 0.82, ROW_Z = 0.26, AISLE = 1.12;
const HOME = new Vector3(-1.1, FLOOR, 2.05); // where the reviewer waits, by the meeting table
// `?close` frames the office tight, for close-ups (a demo video, a small screen).
const CLOSE = new URLSearchParams(location.search).has("close");

// Started after the constants above exist (the bundle turns them into plain vars).
const host = document.getElementById("office3d");
const probe = document.createElement("canvas");
if (host && probe.getContext("webgl2")) start(host);

async function start(host) {
  host.hidden = false;
  const badge = Object.assign(document.createElement("div"), { className: "badge", textContent: "Loading office…" });
  const hint = Object.assign(document.createElement("div"), { className: "hint", textContent: "Run a brief to staff the office" });
  host.append(badge, hint);

  let people, furniture;
  try {
    const loader = new GLTFLoader();
    [people, furniture] = await Promise.all(["/models/people.glb", "/models/furniture.glb"].map((u) => loader.loadAsync(u)));
    const missing = [LEAD, REVIEWER, ...TEAM].filter((n) => !people.scene.getObjectByName("cast_" + n))
      .concat(["desk", "chairDesk", "chair", "computerScreen", "wall", "wallWindow"].filter((n) => !furniture.scene.getObjectByName("kit_" + n)));
    if (missing.length) throw new Error("the models are missing " + missing.join(", "));
  } catch (err) {
    badge.textContent = "3D office unavailable";
    hint.textContent = String(err.message || err);
    return;
  }
  badge.textContent = "WebGL2";

  const mobile = matchMedia("(pointer: coarse)").matches;
  const renderer = new WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, mobile ? 1.5 : 2));
  renderer.outputColorSpace = SRGBColorSpace;
  renderer.toneMapping = ACESFilmicToneMapping;
  renderer.toneMappingExposure = 0.92;
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = PCFSoftShadowMap;
  renderer.xr.enabled = true;
  host.prepend(renderer.domElement);
  // Only offer VR where it works (a headset browser), not a "not supported" button.
  navigator.xr?.isSessionSupported("immersive-vr").then((ok) => ok && host.append(VRButton.createButton(renderer)), () => {});

  const scene = new Scene();
  // Soft, even studio light from the environment, plus one sun for the shadows.
  const pmrem = new PMREMGenerator(renderer);
  scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
  scene.environmentIntensity = 0.35;
  scene.add(new HemisphereLight(0xfff6e8, 0xb9a98c, 0.6));
  const sun = new DirectionalLight(0xffeedb, 2.2);
  sun.position.set(4, 8, 5);
  sun.castShadow = true;
  sun.shadow.mapSize.set(mobile ? 1024 : 2048, mobile ? 1024 : 2048);
  Object.assign(sun.shadow.camera, { left: -6, right: 6, top: 6, bottom: -6, near: 1, far: 20 });
  sun.shadow.bias = -0.0004;
  sun.shadow.normalBias = 0.02;
  sun.shadow.radius = 3;
  scene.add(sun);
  const camera = new PerspectiveCamera(CLOSE ? 34 : 30, 1, 0.1, 80);

  const shadows = (o) => o.traverse((m) => { if (m.isMesh) { m.castShadow = true; m.receiveShadow = true; } });

  // A piece of furniture: a clone (shares geometry and materials), placed and turned.
  // `turn` is in quarter turns; Kenney pieces face +Z.
  function place(name, x, z, turn = 0, parent = scene, y = FLOOR) {
    const p = furniture.scene.getObjectByName("kit_" + name).clone();
    p.position.set(x, y, z);
    p.rotation.y = turn * Math.PI / 2;
    shadows(p);
    parent.add(p);
    return p;
  }
  // Give one material on a clone its own copy, so it can be recolored.
  function own(root, matName, color) {
    let copy = null;
    root.traverse((m) => {
      if (m.isMesh && m.material.name === matName) {
        copy ??= m.material.clone();
        if (color) copy.color.set(color);
        m.material = copy;
      }
    });
    return copy;
  }

  // ---- The room: a wooden floor on a base, two cut-away walls, and its corners ----
  const width = ROOM.x1 - ROOM.x0, depth = ROOM.z1 - ROOM.z0;
  const base = new Mesh(new BoxGeometry(width + 0.3, 0.3, depth + 0.3), new MeshStandardMaterial({ color: "#d4c09c", roughness: 0.9 }));
  base.position.set((ROOM.x0 + ROOM.x1) / 2, -0.15, (ROOM.z0 + ROOM.z1) / 2);
  base.receiveShadow = true;
  // A warm wooden floor in one slab.
  const floor = new Mesh(new BoxGeometry(width, FLOOR, depth), new MeshStandardMaterial({ color: "#d9b48a", roughness: 0.75 }));
  floor.position.set(base.position.x, FLOOR / 2, base.position.z);
  floor.receiveShadow = true;
  scene.add(base, floor);
  // Two walls, at the back and the left; each hides while the camera is behind it.
  const backWall = new Group(), leftWall = new Group();
  scene.add(backWall, leftWall);
  for (let i = 0; i < width; i++) place(i % 3 === 1 ? "wallWindow" : "wall", ROOM.x0 + i + 0.5, ROOM.z0 + 0.03, 0, backWall);
  for (let i = 0; i < depth; i++) place(i % 3 === 1 ? "wallWindow" : "wall", ROOM.x0 + 0.03, ROOM.z0 + i + 0.5, 1, leftWall);

  // Meeting table (the Lead's), lounge, coffee nook, bookcase, plants.
  place("rugRound", -2.0, 1.8);
  place("tableRound", -2.0, 1.8);
  place("chair", -2.0, 1.28, 0); // the Lead's
  place("chair", -2.52, 1.8, 1);
  place("chair", -1.48, 1.8, 3);
  place("laptop", -2.0, 1.7, 2, scene, FLOOR + 0.37);
  place("rugRectangle", -2.55, -1.55, 1);
  place("loungeDesignSofa", -3.1, -1.55, 1);
  place("tableCoffeeGlass", -2.4, -1.55, 1);
  place("loungeChair", -1.75, -0.6, 3);
  place("lampRoundFloor", -3.15, -2.6);
  place("bookcaseOpen", -0.95, -2.8);
  place("bookcaseOpen", -0.5, -2.8);
  place("books", -0.95, -2.8, 0, scene, FLOOR + 0.42);
  place("kitchenFridgeSmall", 2.6, -2.72);
  place("sideTable", 1.95, -2.72);
  place("kitchenCoffeeMachine", 1.8, -2.72, 0, scene, FLOOR + 0.38);
  place("plantSmall1", 2.1, -2.72, 0, scene, FLOOR + 0.38);
  // A reading corner at the front right, so the room's open side isn't bare floor.
  place("rugRound", 2.55, 1.75);
  place("loungeChair", 2.75, 1.95, 3.5);
  place("sideTable", 2.15, 2.2);
  place("lampRoundFloor", 3.2, 2.25);
  place("books", 2.15, 2.2, 1, scene, FLOOR + 0.38);
  for (const [x, z] of [[-3.15, 2.25], [3.15, -2.6], [-0.05, -2.75], [3.2, 1.2], [-3.15, 0.35]]) place("pottedPlant", x, z);

  // ---- Name tags and speech bubbles ----
  const TAG = CLOSE ? 0.75 : 1; // sized for the far view
  function tag(title, state, words = STATE[state][1], color = STATE[state][0], accent = null) {
    const c = Object.assign(document.createElement("canvas"), { width: 512, height: 132 });
    const g = c.getContext("2d");
    g.fillStyle = "rgba(252,250,244,0.96)"; g.strokeStyle = "rgba(23,21,15,0.12)"; g.lineWidth = 3;
    g.beginPath(); g.roundRect(4, 4, 504, 124, 30); g.fill(); g.stroke();
    if (accent) { g.fillStyle = accent; g.beginPath(); g.roundRect(18, 30, 10, 72, 5); g.fill(); } // whose desk
    const chars = Array.from(title); // code points, so an emoji is never cut in half
    g.fillStyle = "#17150f"; g.font = "700 36px 'Schibsted Grotesk', system-ui, sans-serif"; g.textAlign = "center";
    g.fillText(chars.length > 24 ? chars.slice(0, 23).join("") + "…" : title, 262, 56);
    g.font = "600 24px 'Spline Sans Mono', ui-monospace, monospace";
    const w = g.measureText(words.toUpperCase()).width;
    g.fillStyle = color; g.beginPath(); g.arc(262 - w / 2 - 14, 92, 8, 0, Math.PI * 2); g.fill();
    g.fillStyle = "#857d66"; g.textAlign = "left"; g.fillText(words.toUpperCase(), 262 - w / 2 + 4, 100);
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false, toneMapped: false }));
    sprite.scale.set(1.15 * TAG, 0.3 * TAG, 1);
    sprite.position.y = 1.0;
    sprite.renderOrder = 1;
    return sprite;
  }
  const drop = (s) => { if (!s) return; s.parent?.remove(s); s.material.map?.dispose(); s.material.dispose(); };

  function bubble(text) {
    const c = Object.assign(document.createElement("canvas"), { width: 512, height: 190 });
    const g = c.getContext("2d");
    g.font = "500 27px 'Schibsted Grotesk', system-ui, sans-serif";
    const lines = [];
    let line = "";
    for (const word of text.replace(/\s+/g, " ").trim().split(" ")) {
      const next = line ? line + " " + word : word;
      if (g.measureText(next).width > 460 && line) { lines.push(line); line = word; } else line = next;
      if (lines.length === 3) break;
    }
    if (lines.length < 3 && line) lines.push(line);
    if (lines.length === 3 && g.measureText(lines.join(" ")).width < g.measureText(text).width - 1) lines[2] = lines[2].replace(/\s*\S*$/, "") + " …";
    const h = 34 + lines.length * 36;
    g.fillStyle = "#fffdf6"; g.strokeStyle = "rgba(23,21,15,0.16)"; g.lineWidth = 3;
    g.beginPath(); g.roundRect(4, 4, 504, h, 26); g.fill(); g.stroke();
    g.beginPath(); g.moveTo(236, h + 2); g.lineTo(256, h + 26); g.lineTo(276, h + 2); g.fill(); // the tail
    g.fillStyle = "#17150f"; g.textAlign = "center";
    lines.forEach((l, i) => g.fillText(l, 256, 46 + i * 36));
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false, toneMapped: false }));
    sprite.scale.set(1.1 * TAG, 0.41 * TAG, 1);
    sprite.position.y = 1.42;
    sprite.renderOrder = 2;
    return sprite;
  }

  // ---- People: a character with its own animation mixer, that can walk a path ----
  const actors = new Set();
  function actor(id) {
    const root = new Group();
    const body = cloneSkinned(people.scene.getObjectByName("cast_" + id));
    body.position.set(0, 0, 0);
    shadows(body);
    root.add(body);
    const mixer = new AnimationMixer(body);
    const clips = {};
    for (const clip of people.animations) {
      if (clip.name.startsWith(id + "/")) clips[clip.name.slice(id.length + 1)] = clip;
    }
    // `faceCamera`: standing about, they turn to look at the viewer.
    const a = { root, body, mixer, clips, current: null, clip: "", path: [], onArrive: null, label: null, speech: null, faceCamera: true, once: null };
    // A one-shot clip's follow-up runs only if that clip is still the current one.
    mixer.addEventListener("finished", (e) => {
      const once = a.once;
      if (once && e.action === once.action) { a.once = null; once.then(); }
    });
    a.play = (name, { once = false, fade = 0.25, then = null } = {}) => {
      if (a.clip === name && !once) return;
      const next = mixer.clipAction(clips[name]);
      next.reset();
      next.setLoop(once ? LoopOnce : LoopRepeat, once ? 1 : Infinity);
      next.clampWhenFinished = once;
      next.play();
      if (a.current && a.current !== next) a.current.crossFadeTo(next, fade, false);
      a.current = next;
      a.clip = name;
      a.once = once && then ? { action: next, then } : null;
    };
    // Walk through `points` on the floor, then call `then` (or stand idle).
    a.walk = (points, then = null) => {
      a.path = points.map((p) => p.clone().setY(FLOOR));
      a.onArrive = then;
      a.root.position.y = FLOOR;
      a.faceCamera = false;
      if (a.path.length) a.play("walk", { fade: 0.15 });
      else if (then) then();
    };
    a.say = (text, seconds) => {
      drop(a.speech?.sprite);
      a.speech = { sprite: bubble(text), until: performance.now() + seconds * 1000, typing: text === "…" };
      a.root.add(a.speech.sprite);
    };
    a.play("idle", { fade: 0 });
    actors.add(a);
    return a;
  }
  function dispose(a) {
    drop(a.label);
    drop(a.speech?.sprite);
    a.mixer.stopAllAction();
    a.mixer.uncacheRoot(a.body);
    a.body.traverse((m) => { if (m.isSkinnedMesh) m.skeleton.dispose(); });
    a.root.parent?.remove(a.root);
    actors.delete(a);
  }

  // The Lead sits at the meeting table, facing it; the reviewer waits nearby.
  const lead = actor(LEAD);
  lead.root.position.set(-2.0, FLOOR + SEAT_Y, 1.28);
  lead.faceCamera = false;
  lead.play("sit", { fade: 0 });
  const leadTag = () => tag("Lead", "idle", "plans the work", "#d9af1b", "#f2cc46");
  const reviewerTag = () => tag("Reviewer", "idle", "checks drafts", "#6366c9", "#6366c9");
  lead.label = leadTag();
  lead.root.add(lead.label);
  const reviewer = actor(REVIEWER);
  reviewer.root.position.copy(HOME);
  reviewer.label = reviewerTag();
  reviewer.root.add(reviewer.label);
  scene.add(lead.root, reviewer.root);

  // ---- Desks: one per agent (or per task), in a pod of two facing rows ----
  let desks = new Map(); // key → desk
  let owner = new Map(); // task id → desk key
  let staffed = false;
  let verdicts = window.agoraBots?.() ?? {}; // desk key (a member id) → { status, … }
  const deskLayer = new Group();
  scene.add(deskLayer);

  // The monitor's glow: an unlit panel on the screen, and a soft halo seen from any side.
  const screenGeometry = new PlaneGeometry(0.33, 0.17);
  const haloMap = (() => {
    const c = Object.assign(document.createElement("canvas"), { width: 64, height: 64 });
    const g = c.getContext("2d"), r = g.createRadialGradient(32, 32, 0, 32, 32, 32);
    r.addColorStop(0, "rgba(255,255,255,1)"); r.addColorStop(0.45, "rgba(255,255,255,.45)"); r.addColorStop(1, "rgba(255,255,255,0)");
    g.fillStyle = r; g.fillRect(0, 0, 64, 64);
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    return map;
  })();
  function glow(group) {
    const screen = new Mesh(screenGeometry, new MeshBasicMaterial({ color: "#000", toneMapped: false, polygonOffset: true, polygonOffsetFactor: -2 }));
    screen.position.set(0, 0.38 + 0.185, -0.1 + 0.03);
    const halo = new Sprite(new SpriteMaterial({ map: haloMap, transparent: true, depthWrite: false, toneMapped: false, opacity: 0.8 }));
    halo.position.set(0, 0.38 + 0.2, -0.1);
    halo.scale.set(0.95, 0.75, 1);
    screen.visible = halo.visible = false;
    group.add(screen, halo);
    return { screen, halo };
  }
  // Light (or darken) one desk's monitor from its bot's latest errand.
  function showVerdict(d) {
    const v = VERDICT[verdicts[d.key]?.status];
    d.glow.screen.visible = d.glow.halo.visible = !!v;
    d.pulse = verdicts[d.key]?.status === "running";
    if (!d.pulse) d.glow.halo.material.opacity = 0.8;
    if (v) { d.glow.screen.material.color.set(v[0]); d.glow.halo.material.color.set(v[0]); }
    const words = v && d.state === "free" ? v[1] : "";
    if (words !== (d.verdictWords ?? "")) { d.verdictWords = words; relabel(d); }
  }
  function relabel(d) {
    drop(d.person.label);
    const v = d.verdictWords && VERDICT[verdicts[d.key]?.status];
    d.person.label = v ? tag(d.title, d.state, v[1], v[0], d.color) : tag(d.title, d.state, undefined, undefined, d.color);
    d.person.root.add(d.person.label);
  }

  function clearDesks() {
    for (const d of desks.values()) { dispose(d.person); d.cushion?.dispose(); d.glow.screen.material.dispose(); d.glow.halo.material.dispose(); }
    deskLayer.clear();
    desks = new Map();
    hint.hidden = false;
  }

  // Desk slot i of n: the back row first (its people face the camera), then the front.
  function slot(i, n) {
    const perRow = Math.ceil(n / 2);
    const row = i < perRow ? 0 : 1;
    const k = row === 0 ? i : i - perRow;
    const count = row === 0 ? perRow : n - perRow;
    const x = POD_X + (k - (count - 1) / 2) * DESK_GAP;
    const out = row === 0 ? -1 : 1; // which side the sitter is on
    return { x, out };
  }

  function build(entries) {
    clearDesks();
    hint.hidden = true;
    const n = entries.length;
    entries.forEach((entry, i) => {
      const { x, out } = slot(i, n);
      // The desk's front (the sitter's side) faces out of the pod.
      const group = new Group();
      group.position.set(x, FLOOR, out * ROW_Z);
      group.rotation.y = out < 0 ? Math.PI : 0;
      place("desk", 0, 0, 0, group, 0);
      place("computerScreen", 0, -0.1, 0, group, 0.38);
      place("computerKeyboard", 0, 0.06, 0, group, 0.38);
      place("computerMouse", 0.2, 0.06, 0, group, 0.38);
      if (i % 3 === 1) place("plantSmall2", -0.27, -0.08, 0, group, 0.38);
      const chair = place("chairDesk", 0, 0.42, 2, group, 0); // turned to face the desk
      const cushion = own(chair, "carpet", entry.color);
      const lit = glow(group);
      group.userData.desk = entry.key; // what a click on it picks
      deskLayer.add(group);

      // The character picked for the agent in the team editor, or one by seat.
      const person = actor(TEAM.includes(entry.character) ? entry.character : TEAM[i % TEAM.length]);
      const seat = new Vector3(x, FLOOR + SEAT_Y, out * (ROW_Z + 0.36));
      const stand = new Vector3(x, FLOOR, out * AISLE);
      person.root.position.copy(stand);
      person.root.userData.desk = entry.key;
      deskLayer.add(person.root);
      const d = { key: entry.key, title: entry.title, color: entry.color, person, cushion, seat, stand, out, group, glow: lit,
        state: "", tasks: new Map(), seated: false };
      refresh(d);
      showVerdict(d);
      desks.set(entry.key, d);
    });
  }

  // A path that keeps to the aisles: step out of the pod row, go around the pod's
  // end if the target is on the other side, then along the target's aisle.
  const aisle = (z) => (z < 0 ? -AISLE : AISLE);
  function route(from, to) {
    const pts = [];
    if (Math.abs(from.z) < AISLE - 0.05) pts.push(new Vector3(from.x, 0, aisle(from.z)));
    if ((from.z < 0) !== (to.z < 0)) {
      const xs = [...desks.values()].map((d) => d.seat.x);
      const end = (xs.length ? Math.min(...xs) : POD_X) - 0.75;
      pts.push(new Vector3(end, 0, aisle(from.z)), new Vector3(end, 0, aisle(to.z)));
    }
    if (Math.abs(to.z) < AISLE - 0.05) pts.push(new Vector3(to.x, 0, aisle(to.z)));
    pts.push(to);
    return pts;
  }

  // What a desk shows: the most pressing state among its tasks.
  function refresh(d) {
    const states = [...d.tasks.values()];
    const state = states.length ? RANK.find((r) => states.includes(r)) : "free";
    if (state === d.state) return;
    const was = d.state;
    d.state = state;
    if (d.glow) { d.verdictWords = d.state === "free" ? VERDICT[verdicts[d.key]?.status]?.[1] ?? "" : ""; }
    relabel(d);
    act(d, was);
  }

  // Move the teammate to fit the new state: sit to work, stand to celebrate or worry.
  function act(d, was) {
    const p = d.person;
    if (SEATED.has(d.state)) {
      if (d.seated || d.toSeat) return; // there, or on the way
      d.toSeat = true;
      p.walk([d.stand, d.seat], () => {
        d.toSeat = false;
        p.root.position.copy(d.seat);
        p.root.rotation.y = d.out < 0 ? 0 : Math.PI; // facing the desk
        p.play("sit");
        d.seated = true;
      });
      return;
    }
    // Not sitting any more: stop any walk to the chair, and stand in the aisle.
    p.path = [];
    p.onArrive = null;
    d.toSeat = false;
    if (d.seated || p.root.position.distanceTo(d.stand) > 0.05) {
      d.seated = false;
      p.root.position.copy(d.stand);
    }
    p.faceCamera = true;
    if (d.state === "accepted" && was && was !== "accepted") p.play("emote-yes", { once: true, then: () => p.play("idle") });
    else if (d.state === "failed") p.play("emote-no", { once: true, then: () => p.play("idle") });
    else p.play("idle");
  }

  // ---- The reviewer: walks to a draft, gestures at the screen, walks back ----
  function review(d) {
    const spot = new Vector3(d.seat.x + 0.42, FLOOR, d.out * (ROW_Z + 0.48));
    reviewer.walk(route(reviewer.root.position, spot), () => {
      reviewer.root.lookAt(d.seat.x, reviewer.root.position.y, d.seat.z);
      reviewer.play("interact-right");
    });
  }
  function goHome() {
    reviewer.walk(route(reviewer.root.position, HOME), () => { reviewer.faceCamera = true; reviewer.play("idle"); });
  }

  function onEvent(e) {
    const d = e.task && desks.get(owner.get(e.task));
    const mark = (state) => { d.tasks.set(e.task, state); refresh(d); };
    switch (e.type) {
      case "staffed":
        staffed = true;
        build(e.team.agents.map((a) => ({ key: a.id, title: a.name, color: a.color, character: a.character })));
        break;
      case "planned":
        if (!staffed) build(e.tasks.map((t, i) => ({ key: t.id, title: t.title, color: PALETTE[i % PALETTE.length] })));
        owner = new Map(e.tasks.map((t) => [t.id, staffed && desks.has(t.agent) ? t.agent : t.id]));
        for (const t of e.tasks) desks.get(owner.get(t.id))?.tasks.set(t.id, "idle");
        for (const desk of desks.values()) refresh(desk);
        lead.say(`The plan: ${e.tasks.length} task${e.tasks.length === 1 ? "" : "s"}. Let's go!`, 5);
        break;
      case "task_started": if (d) mark("working"); break;
      case "drafted": if (d) { mark("review"); review(d); } break;
      case "reviewed": if (d && e.unmet.length) mark("working"); break;
      case "task_accepted": if (d) mark("accepted"); break;
      case "task_failed": if (d) mark("failed"); break;
      case "task_skipped": if (d) mark("skipped"); break;
      case "finished": case "run_failed": goHome(); break;
    }
  }

  // Tags drawn before the web fonts arrived used a fallback font: redraw them once.
  document.fonts?.ready.then(() => {
    for (const d of desks.values()) relabel(d);
    for (const [who, make] of [[lead, leadTag], [reviewer, reviewerTag]]) { drop(who.label); who.label = make(); who.root.add(who.label); }
  });

  // Once a run is being watched, its events decide who is in the office; before
  // that, the team picked on the page sits at the desks.
  let watching = false;
  function showTeam(team) {
    if (watching) return;
    if (team) onEvent({ type: "staffed", team });
    else clearDesks();
  }
  addEventListener("agora:reset", () => { watching = true; clearDesks(); owner = new Map(); staffed = false; goHome(); });
  addEventListener("agora:event", (ev) => { watching = true; onEvent(ev.detail); });
  addEventListener("agora:team", (ev) => showTeam(ev.detail));

  // Speech bubbles: whoever is talking in the chat says it over their head.
  const speaker = (e) => (e.agent ? desks.get(e.agent)?.person : e.from === "Lead" || e.message?.kind === "lead" ? lead : null);
  addEventListener("agora:chat", ({ detail: e }) => {
    if (e.type === "typing") speaker(e)?.say("…", 30);
    else if (e.type === "idle") for (const a of actors) { if (a.speech?.typing) a.speech.until = 0; } // nobody is typing now
    else if (e.type === "message" && Date.now() - e.message.at_ms < 20000) speaker({ agent: e.message.agent, message: e.message })?.say(e.message.text, 9);
  });


  // ---- Alibi errands: for each call the agent goes to the records shelf (reads) or
  // the laptop at the meeting table (sending, booking, writing). When its account is
  // checked, a ring flashes under it: red for contradicted claims, purple for things
  // it did but didn't mention, green when every claim checks out; on a red one the
  // reviewer walks over as the auditor.
  const SHELF = new Vector3(-0.72, FLOOR, -2.3), LAPTOP = new Vector3(-1.32, FLOOR, 2.3);
  const ACTS = new Set(["calendar.create", "email.send", "messages.send", "files.write"]);
  const rings = [];
  function flash(at, color) {
    const ring = new Mesh(new RingGeometry(0.22, 0.34, 40), new MeshBasicMaterial({ color, transparent: true, opacity: 0.9, depthWrite: false }));
    ring.rotation.x = -Math.PI / 2;
    ring.position.set(at.x, FLOOR + 0.006, at.z);
    scene.add(ring);
    rings.push({ ring, born: performance.now() });
  }
  function errandDesk(id) {
    if (!desks.size) build([{ key: "scout", title: "Scout", color: PALETTE[0] }]);
    return desks.get(id) ?? desks.values().next().value;
  }
  function goTo(d, spot, then) {
    const p = d.person;
    d.seated = false; d.toSeat = false;
    if (p.root.position.y > FLOOR + 0.01) p.root.position.copy(d.stand); // up from the chair first
    p.walk(route(p.root.position, spot), then);
  }
  let guarded = true; // the errand's guard setting (from its `started` event)
  function onErrand(e) {
    if (e.type === "started") guarded = e.guard !== false;
    const d = errandDesk(e.agentId);
    const p = d.person;
    switch (e.type) {
      case "busy":
        p.say(e.turn === 0 ? "On it!" : "…", e.turn === 0 ? 3 : 30);
        break;
      case "step": {
        const { action, outcome } = e.entry;
        goTo(d, ACTS.has(action.tool) ? LAPTOP : SHELF, () => { p.faceCamera = false; p.play("interact-right"); });
        p.say(`${action.tool} · ${outcome.subject}${outcome.ok ? "" : " ✗"}`, 4);
        break;
      }
      case "draft": {
        // A draft the guard stopped: caught, and sent back (or corrected).
        const c = e.check;
        if (!guarded || (!c.false_claims.length && !c.unmentioned.length && !c.unverified)) break; // guard off: nothing is stopped
        flash(p.root.position, c.false_claims.length ? "#e5483b" : c.unverified ? "#f2a81d" : "#6b4fd8");
        goTo(d, d.stand, () => { p.faceCamera = true; p.play("emote-no", { once: true, then: () => p.play("idle") }); });
        p.say(c.false_claims.length ? `The ledger contradicts ${c.false_claims.length} of my claim${c.false_claims.length === 1 ? "" : "s"}…`
          : c.unmentioned.length ? "I left something out…" : "That couldn't be fully checked…", 6);
        reviewer.walk(route(reviewer.root.position, new Vector3(d.stand.x + 0.45, FLOOR, d.stand.z + (d.stand.z < 0 ? -0.2 : 0.2))), () => {
          reviewer.root.lookAt(d.stand.x, reviewer.root.position.y, d.stand.z);
          reviewer.play("interact-right");
          reviewer.say("That's not what the ledger says. Rewrite it.", 5);
        });
        break;
      }
      case "said": {
        goTo(d, d.stand, () => { p.faceCamera = true; p.play("emote-yes", { once: true, then: () => p.play("idle") }); });
        const lies = e.check.audit.checked.filter((c, i) => c.verdict === "contradicted" && e.check.agreed[i] !== false).length;
        flash(p.root.position, lies ? "#e5483b" : "#1f9d55");
        p.say(lies ? `${lies} claim${lies === 1 ? "" : "s"} the ledger contradicts (guard off)` : e.correction ? "Corrected from the ledger" : "Every claim checks out", 7);
        setTimeout(goHome, 2500);
        break;
      }
      case "failed": p.say("I couldn't finish that.", 6); break;
      case "forgotten":
        flash(p.root.position, "#f2cc46");
        p.say(`Forgotten. The ledger still proves entry #${e.receipt.seq} happened.`, 7);
        break;
    }
  }
  addEventListener("agora:errand", ({ detail }) => onErrand(detail));
  addEventListener("agora:bots", ({ detail }) => { verdicts = detail || {}; for (const d of desks.values()) showVerdict(d); });

  // ---- Camera: drag to turn all the way round and tilt, wheel or pinch to zoom.
  // Left alone, it drifts slowly round the room; touching it pauses the drift.
  const look = new Vector3(0.1, CLOSE ? 0.45 : 0.2, 0.2);
  const [startDistance, startHeight] = CLOSE ? [5.2, 2.7] : [8.9, 6.1];
  camera.position.set(look.x + Math.cos(0.8) * startDistance, startHeight, look.z + Math.sin(0.8) * startDistance);
  const controls = new OrbitControls(camera, renderer.domElement);
  controls.target.copy(look);
  Object.assign(controls, {
    enableDamping: true, dampingFactor: 0.08, enablePan: false, rotateSpeed: 0.7, zoomSpeed: 0.8,
    minDistance: 2.5, maxDistance: 15,
    minPolarAngle: 0.15, maxPolarAngle: 1.38, // from nearly overhead down to just above the floor
    autoRotate: true, autoRotateSpeed: 0.35,
  });
  let resumeDrift = 0;
  controls.addEventListener("start", () => { controls.autoRotate = false; clearTimeout(resumeDrift); });
  controls.addEventListener("end", () => { resumeDrift = setTimeout(() => { controls.autoRotate = true; }, 6000); });
  controls.update();

  // A click (not a drag) on a desk or the person at it picks that bot.
  const ray = new Raycaster(), pointer = new Vector2();
  let downAt = null;
  renderer.domElement.addEventListener("pointerdown", (e) => { downAt = [e.clientX, e.clientY]; });
  renderer.domElement.addEventListener("pointerup", (e) => {
    if (!downAt || Math.hypot(e.clientX - downAt[0], e.clientY - downAt[1]) > 6) return;
    downAt = null;
    const box = renderer.domElement.getBoundingClientRect();
    pointer.set(((e.clientX - box.left) / box.width) * 2 - 1, -((e.clientY - box.top) / box.height) * 2 + 1);
    ray.setFromCamera(pointer, camera);
    const targets = [...desks.values()].flatMap((d) => [d.group, d.person.root]);
    let o = ray.intersectObjects(targets, true)[0]?.object;
    while (o && o.userData.desk === undefined) o = o.parent;
    if (o) dispatchEvent(new CustomEvent("agora:pick", { detail: { id: o.userData.desk, name: desks.get(o.userData.desk)?.title } }));
  });

  function resize() {
    const w = host.clientWidth, h = Math.max(300, host.clientHeight);
    renderer.setSize(w, h, false);
    renderer.domElement.style.width = "100%";
    renderer.domElement.style.height = h + "px";
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
  new ResizeObserver(resize).observe(host);
  resize();

  const step = new Vector3();
  const turnToward = (a, x, z, dt) => {
    const want = Math.atan2(x - a.root.position.x, z - a.root.position.z);
    const turn = Math.atan2(Math.sin(want - a.root.rotation.y), Math.cos(want - a.root.rotation.y));
    a.root.rotation.y += turn * Math.min(1, dt * 10);
  };
  let last = performance.now();
  const loop = (now) => {
    const dt = Math.min((now - last) / 1000, 0.1);
    last = now;
    if (!renderer.xr.isPresenting) controls.update(dt);
    // A wall the camera is behind would hide the room: leave it out of the picture.
    backWall.visible = camera.position.z > ROOM.z0;
    leftWall.visible = camera.position.x > ROOM.x0;
    for (const a of actors) {
      a.mixer.update(dt);
      if (a.path.length) {
        // Walking: head for the next point, turning smoothly toward it.
        const next = a.path[0];
        step.subVectors(next, a.root.position).setY(0);
        const dist = step.length();
        if (dist < 0.02) {
          a.path.shift();
          if (!a.path.length) {
            const then = a.onArrive;
            a.onArrive = null;
            if (then) then(); else { a.faceCamera = true; a.play("idle"); }
          }
        } else {
          a.root.position.addScaledVector(step.divideScalar(dist), Math.min(dist, dt * WALK_SPEED));
          turnToward(a, next.x, next.z, dt);
        }
      } else if (a.faceCamera) turnToward(a, camera.position.x, camera.position.z, dt);
      if (a.speech && now > a.speech.until) { drop(a.speech.sprite); a.speech = null; }
    }
    // A bot on an errand: its monitor breathes.
    for (const d of desks.values()) if (d.pulse) d.glow.halo.material.opacity = 0.45 + 0.4 * (0.5 + 0.5 * Math.sin(now / 320));
    // Errand rings grow and fade over three seconds.
    for (let i = rings.length - 1; i >= 0; i--) {
      const r = rings[i], age = (now - r.born) / 3000;
      if (age >= 1) { scene.remove(r.ring); r.ring.geometry.dispose(); r.ring.material.dispose(); rings.splice(i, 1); continue; }
      r.ring.scale.setScalar(1 + age * 1.6);
      r.ring.material.opacity = 0.9 * (1 - age);
    }
    renderer.render(scene, camera);
  };
  // Draw only while the office is on screen (or in VR); scrolled away, the GPU rests.
  new IntersectionObserver(([entry]) => {
    last = performance.now();
    renderer.setAnimationLoop(entry.isIntersecting || renderer.xr.isPresenting ? loop : null);
  }).observe(host);

  // This script can load after the run's history has already streamed in: catch up
  // (last, once everything the handlers touch exists).
  const history = window.agoraHistory?.() ?? [];
  if (history.length) watching = true;
  for (const e of history) onEvent(e);
  showTeam(window.agoraIdleTeam?.() ?? null);
}
