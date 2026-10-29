// The 3D office: a small diorama of a studio where the run's team works. Each agent
// (or each task, without a team) has a desk in the pod; they stand in the aisle while
// their work waits, walk to their chair and sit to work, and stand up to cheer when
// the work is accepted. The Lead sits at the meeting table; the reviewer walks the
// aisles to whoever has a draft.
//
// It only listens: the page dispatches `agora:event` (one run event), `agora:reset`
// (a run is about to replay), `agora:team` (the team picked while no run is open)
// and `agora:chat` (chat traffic, shown as speech bubbles).
//
// Art: Kenney's Mini Characters and Furniture Kit (CC0, kenney.nl), trimmed into two
// files by web/models/build.mjs. Without WebGL2 the page keeps its kanban.

import {
  ACESFilmicToneMapping, AnimationMixer, BoxGeometry, CanvasTexture, DirectionalLight, Group, HemisphereLight,
  LoopOnce, LoopRepeat, Mesh, MeshStandardMaterial, PCFSoftShadowMap, PerspectiveCamera, PMREMGenerator, Scene,
  SRGBColorSpace, Sprite, SpriteMaterial, Vector3, WebGLRenderer,
} from "three";
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
  for (let i = 0; i < width; i++) place(i % 3 === 1 ? "wallWindow" : "wall", ROOM.x0 + i + 0.5, ROOM.z0 + 0.03);
  for (let i = 0; i < depth; i++) place(i % 3 === 1 ? "wallWindow" : "wall", ROOM.x0 + 0.03, ROOM.z0 + i + 0.5, 1);

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
  const deskLayer = new Group();
  scene.add(deskLayer);

  function clearDesks() {
    for (const d of desks.values()) { dispose(d.person); d.cushion?.dispose(); }
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
      deskLayer.add(group);

      const person = actor(TEAM[i % TEAM.length]);
      const seat = new Vector3(x, FLOOR + SEAT_Y, out * (ROW_Z + 0.36));
      const stand = new Vector3(x, FLOOR, out * AISLE);
      person.root.position.copy(stand);
      deskLayer.add(person.root);
      const d = { key: entry.key, title: entry.title, color: entry.color, person, cushion, seat, stand, out,
        state: "", tasks: new Map(), seated: false };
      refresh(d);
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
    drop(d.person.label);
    d.person.label = tag(d.title, state, undefined, undefined, d.color);
    d.person.root.add(d.person.label);
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
        build(e.team.agents.map((a) => ({ key: a.id, title: a.name, color: a.color })));
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
    for (const d of desks.values()) {
      drop(d.person.label);
      d.person.label = tag(d.title, d.state, undefined, undefined, d.color);
      d.person.root.add(d.person.label);
    }
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

  // ---- Camera: a gentle sway over the diorama's open side; drag to look around ----
  const orbit = { angle: 0.8, distance: CLOSE ? 5.2 : 8.9, height: CLOSE ? 2.7 : 6.1, dragging: false, lastX: 0, drift: 1 };
  const MIN_ANGLE = 0.25, MAX_ANGLE = 1.32; // the walls are on two sides: stay on the open ones
  renderer.domElement.addEventListener("pointerdown", (e) => {
    orbit.dragging = true;
    orbit.lastX = e.clientX;
    renderer.domElement.setPointerCapture(e.pointerId);
  });
  const stopDrag = () => { orbit.dragging = false; };
  renderer.domElement.addEventListener("pointerup", stopDrag);
  renderer.domElement.addEventListener("pointercancel", stopDrag);
  addEventListener("pointermove", (e) => {
    if (!orbit.dragging) return;
    orbit.angle -= (e.clientX - orbit.lastX) * 0.006;
    orbit.lastX = e.clientX;
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

  const look = new Vector3(0.1, CLOSE ? 0.45 : 0.2, 0.2);
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
    if (!orbit.dragging) {
      orbit.angle += dt * 0.025 * orbit.drift;
      if (orbit.angle > MAX_ANGLE - 0.05) orbit.drift = -1;
      if (orbit.angle < MIN_ANGLE + 0.05) orbit.drift = 1;
    }
    orbit.angle = Math.min(MAX_ANGLE, Math.max(MIN_ANGLE, orbit.angle));
    if (!renderer.xr.isPresenting) {
      camera.position.set(look.x + Math.cos(orbit.angle) * orbit.distance, orbit.height, look.z + Math.sin(orbit.angle) * orbit.distance);
      camera.lookAt(look);
    }
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
