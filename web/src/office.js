// The 3D office: a desk and chair per task, a teammate at each, a Lead at the planning
// table, and the purple reviewer walking to whichever desk has a draft to check.
//
// It only listens: the board page dispatches `agora:event` (one run event) and
// `agora:reset` (a run is about to replay from the start). The furniture and the
// teammate are built in Blender (assets/blender/build_assets.py) and loaded as glTF;
// the floor is drawn here. Without WebGL2 the page keeps its kanban.

import {
  BoxGeometry, CanvasTexture, Color, DirectionalLight, GridHelper, Group, HemisphereLight, Line, LineBasicMaterial,
  BufferGeometry, Mesh, MeshLambertMaterial, PerspectiveCamera, PlaneGeometry, Scene, SRGBColorSpace, Sprite,
  SpriteMaterial, Vector3, WebGLRenderer,
} from "three";
import { GLTFLoader } from "three/addons/loaders/GLTFLoader.js";
import { VRButton } from "three/addons/webxr/VRButton.js";

const PALETTE = window.agoraPalette ?? ["#4285f4", "#34a853", "#ea4335", "#14b8a6", "#f97316", "#ec4899", "#64748b", "#a16207"];
const LEAD = "#f2cc46", REVIEWER = "#6366c9";
const STATE = {
  idle: ["#b3aa8f", "scheduled"], working: ["#4285f4", "working"], review: ["#d9af1b", "in review"],
  accepted: ["#1f9d55", "done"], failed: ["#e5483b", "needs a person"], skipped: ["#b3aa8f", "skipped"],
  free: ["#b3aa8f", "available"], // an agent with no task on this run
};
const SEATED = new Set(["working", "review"]);
const HOME = new Vector3(0, 0, 2.35); // where the reviewer waits, between two chairs at the planning table

// Started after the constants above exist (the bundle turns them into plain vars).
const host = document.getElementById("office3d");
const probe = document.createElement("canvas");
if (host && probe.getContext("webgl2")) start(host);

async function start(host) {
  host.hidden = false;
  const badge = Object.assign(document.createElement("div"), { className: "badge", textContent: "Loading office…" });
  const hint = Object.assign(document.createElement("div"), { className: "hint", textContent: "Run a brief to staff the office" });
  host.append(badge, hint);

  let kit, chibi;
  try {
    const loader = new GLTFLoader();
    [kit, chibi] = await Promise.all(["/models/office-kit.glb", "/models/chibi.glb"].map((u) => loader.loadAsync(u)));
    // Everything below looks pieces and materials up by name: check them all now.
    const names = new Set();
    for (const g of [kit, chibi]) g.scene.traverse((o) => { names.add(o.name); if (o.material) names.add("mat:" + o.material.name); });
    const missing = ["Desk", "Chair", "RoundTable", "Sofa", "CoffeeTable", "Lamp", "Whiteboard", "Shelf", "Cooler", "Plant", "Chibi",
      "Head", "Body", "ArmL", "ArmR", "LegL", "LegR", "mat:Skin", "mat:Accent", "mat:Screen"].filter((n) => !names.has(n));
    if (missing.length) throw new Error("the models are missing " + missing.join(", "));
  } catch (err) {
    badge.textContent = "3D office unavailable";
    hint.textContent = String(err.message || err);
    return;
  }
  badge.textContent = "WebGL2";

  const renderer = new WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.outputColorSpace = SRGBColorSpace;
  renderer.xr.enabled = true;
  host.prepend(renderer.domElement);
  // Only offer VR where it works (a headset browser), not a "not supported" button.
  navigator.xr?.isSessionSupported("immersive-vr").then((ok) => ok && host.append(VRButton.createButton(renderer)), () => {});

  const scene = new Scene();
  scene.add(new HemisphereLight(0xffffff, 0xcfc6b0, 2.6));
  const sun = new DirectionalLight(0xffffff, 2.2);
  sun.position.set(5, 11, 7);
  scene.add(sun);
  const camera = new PerspectiveCamera(36, 1, 0.1, 80);

  // The floor: a white tiled slab on a blue rim.
  const SIZE = 17;
  const rim = new Mesh(new BoxGeometry(SIZE + 0.35, 0.28, SIZE + 0.35), new MeshLambertMaterial({ color: "#3b82f6" }));
  rim.position.y = -0.17;
  const slab = new Mesh(new BoxGeometry(SIZE, 0.3, SIZE), new MeshLambertMaterial({ color: "#f7f5ef" }));
  slab.position.y = -0.15;
  const grid = new GridHelper(SIZE, 34, 0xe3ddcf, 0xe3ddcf);
  grid.position.y = 0.002;
  scene.add(rim, slab, grid);

  // Pieces from the kit. A clone shares geometry and materials with the original.
  const piece = (name) => {
    const p = kit.scene.getObjectByName(name);
    if (!p) throw new Error(`office kit has no ${name}`);
    return p.clone();
  };
  const place = (name, x, z, turn = 0) => {
    const p = piece(name);
    p.position.set(x, 0, z);
    p.rotation.y = turn;
    scene.add(p);
    return p;
  };
  // Recolor one named material on a clone, giving the clone its own copy.
  const recolor = (root, matName, color) => {
    let copy = null;
    root.traverse((m) => {
      if (m.isMesh && m.material.name === matName) {
        copy ??= m.material.clone();
        if (color) copy.color.set(color);
        m.material = copy;
      }
    });
    return copy;
  };

  // The planning table with its chairs, and the room's corners.
  place("RoundTable", 0, 0);
  for (let i = 0; i < 5; i++) {
    const a = (i / 5) * Math.PI * 2 - Math.PI / 2; // the first is the Lead's, at z = -1.55
    place("Chair", Math.cos(a) * 1.55, Math.sin(a) * 1.55).lookAt(0, 0, 0);
  }
  place("Sofa", -6.4, 7.6, Math.PI);
  place("CoffeeTable", -6.3, 5.9);
  place("Lamp", -8.0, 7.9);
  place("Whiteboard", 5.9, -7.7);
  place("Shelf", -6.2, -8.0);
  place("Cooler", 7.9, 6.2, -Math.PI / 2);
  for (const [x, z] of [[-8, 4.6], [7.7, 7.8], [3.5, -8], [-8, -8], [8, -4.6]]) place("Plant", x, z);

  const shadowTex = (() => {
    const c = Object.assign(document.createElement("canvas"), { width: 64, height: 64 });
    const g = c.getContext("2d");
    const r = g.createRadialGradient(32, 32, 0, 32, 32, 32);
    r.addColorStop(0, "rgba(23,21,15,0.3)"); r.addColorStop(1, "rgba(23,21,15,0)");
    g.fillStyle = r; g.fillRect(0, 0, 64, 64);
    return new CanvasTexture(c);
  })();
  const shadowGeo = new PlaneGeometry(1.1, 1.1);
  const shadowMat = new MeshLambertMaterial({ map: shadowTex, transparent: true, depthWrite: false });

  // A teammate: the Blender character in its own color, its parts ready to animate.
  function teammate(color) {
    const c = chibi.scene.getObjectByName("Chibi").clone();
    const skin = recolor(c, "Skin", color);
    const part = (n) => c.getObjectByName(n);
    const shadow = new Mesh(shadowGeo, shadowMat);
    shadow.rotation.x = -Math.PI / 2;
    shadow.position.y = 0.01; // just above the floor, so it never flickers against it
    c.add(shadow);
    Object.assign(c.userData, {
      skin, color, shadow, head: part("Head"), body: part("Body"),
      arms: [part("ArmL"), part("ArmR")], legs: [part("LegL"), part("LegR")], pose: 0,
    });
    return c;
  }

  // Name tag: a title, and a colored dot with what the teammate is doing.
  function tag(title, state, words = STATE[state][1], color = STATE[state][0]) {
    const c = Object.assign(document.createElement("canvas"), { width: 512, height: 132 });
    const g = c.getContext("2d");
    g.fillStyle = "rgba(252,250,244,0.96)"; g.strokeStyle = "rgba(23,21,15,0.12)"; g.lineWidth = 3;
    g.beginPath(); g.roundRect(4, 4, 504, 124, 30); g.fill(); g.stroke();
    const chars = Array.from(title); // code points, so an emoji is never cut in half
    g.fillStyle = "#17150f"; g.font = "700 36px 'Schibsted Grotesk', system-ui, sans-serif"; g.textAlign = "center";
    g.fillText(chars.length > 24 ? chars.slice(0, 23).join("") + "…" : title, 256, 56);
    g.font = "600 24px 'Spline Sans Mono', ui-monospace, monospace";
    const w = g.measureText(words.toUpperCase()).width;
    g.fillStyle = color; g.beginPath(); g.arc(256 - w / 2 - 14, 92, 8, 0, Math.PI * 2); g.fill();
    g.fillStyle = "#857d66"; g.textAlign = "left"; g.fillText(words.toUpperCase(), 256 - w / 2 + 4, 100);
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false }));
    sprite.scale.set(3.2, 0.82, 1);
    sprite.position.y = 2.3;
    return sprite;
  }
  const dropTag = (s) => { s.parent?.remove(s); s.material.map?.dispose(); s.material.dispose(); };

  // The Lead plans at the round table; the reviewer waits beside it.
  const lead = teammate(LEAD);
  lead.position.set(0, 0.22, -1.55);
  lead.lookAt(0, 0.22, 0);
  const leadTag = () => tag("Lead", "idle", "plans the work", LEAD);
  const reviewerTag = () => tag("Reviewer", "idle", "checks drafts", REVIEWER);
  lead.add(leadTag());
  const reviewer = teammate(REVIEWER);
  reviewer.position.copy(HOME);
  reviewer.add(reviewerTag());
  scene.add(lead, reviewer);
  let reviewerTarget = HOME.clone();

  // One desk per agent when the run has a team, else one per task. Each task maps
  // to a desk, and a desk shows the most pressing state among its tasks.
  let desks = new Map(); // desk key → { person, screen, cushion, label, title, state, tasks, hop, seat, stand, out, look }
  let owner = new Map(); // task id → desk key
  let staffed = false;
  const deskLayer = new Group();
  scene.add(deskLayer);

  function clearDesks() {
    deskLayer.traverse((m) => {
      if (m.isSprite) { m.material.map?.dispose(); m.material.dispose(); }
      if (m.isLine) { m.geometry.dispose(); m.material.dispose(); }
    });
    // Each desk's own material copies; the kit's shared ones stay.
    for (const d of desks.values()) { d.person.userData.skin.dispose(); d.screen?.dispose(); d.cushion?.dispose(); }
    for (const [who] of speaking) if (who.parent === deskLayer) speaking.delete(who); // their sprites are disposed above
    deskLayer.clear();
    desks = new Map();
    hint.hidden = false;
  }

  function setState(d, state) {
    d.state = state;
    dropTag(d.label);
    d.label = tag(d.title, state);
    d.person.add(d.label);
    const { skin, color } = d.person.userData;
    skin.color.set(state === "skipped" ? "#d9d3c4" : color); // skipped: greyed out
    // The monitor glows in the state's color while there is something to show.
    const dark = state === "idle" || state === "skipped" || state === "free";
    if (d.screen) d.screen.emissive.set(dark ? new Color("#191c28") : new Color(STATE[state][0]).multiplyScalar(0.55));
  }

  const RANK = ["review", "working", "failed", "idle", "accepted", "skipped"];
  function refresh(d) {
    const states = [...d.tasks.values()];
    const state = states.length ? RANK.find((r) => states.includes(r)) : "free";
    if (state !== d.state) setState(d, state);
  }

  // A ring of desks, one per entry ({ key, title, color }).
  function build(entries) {
    clearDesks();
    hint.hidden = true;
    const n = entries.length;
    const radius = Math.min(6.3, Math.max(4.4, n * 0.85)); // at most 8 desks (8 tasks or 6 agents)
    entries.forEach((entry, i) => {
      const angle = (i / n) * Math.PI * 2 - Math.PI / 2;
      const out = new Vector3(Math.cos(angle), 0, Math.sin(angle));
      const side = new Vector3(-out.z, 0, out.x);
      // The teammate sits on the inside of the ring, facing out over the desk.
      const desk = piece("Desk");
      desk.position.copy(out).multiplyScalar(radius);
      desk.lookAt(0, 0, 0); // the sitter's side (+Z) toward the middle
      const chair = piece("Chair");
      chair.position.copy(out).multiplyScalar(radius - 0.78);
      chair.lookAt(out.clone().multiplyScalar(radius * 2));
      const cushion = recolor(chair, "Accent", entry.color);
      const screen = recolor(desk, "Screen");
      deskLayer.add(desk, chair);

      const person = teammate(entry.color);
      const seat = chair.position.clone().setY(0.22);
      const stand = chair.position.clone().addScaledVector(side, 0.95).addScaledVector(out, -0.15);
      person.position.copy(stand);
      deskLayer.add(person);
      const look = seat.clone().add(out); // where a seated teammate looks: the monitor
      const d = { person, screen, cushion, label: new Sprite(), title: entry.title, state: "", tasks: new Map(), hop: 0, seat, stand, out, look };
      refresh(d);
      desks.set(entry.key, d);
    });
    orbit.distance = radius * 1.5 + 7;
    orbit.height = radius * 1.1 + 6.5;
  }

  // Faint blue lines on the floor for "this desk needs that one's output".
  function links(pairs) {
    const lineMat = new LineBasicMaterial({ color: 0x8fb3f5 });
    const drawn = new Set();
    for (const [a, b] of pairs) {
      const from = desks.get(a), to = desks.get(b);
      if (!from || !to || from === to || drawn.has(a + ">" + b)) continue;
      drawn.add(a + ">" + b);
      const pts = [from.seat.clone().setY(0.03), to.seat.clone().setY(0.03)];
      deskLayer.add(new Line(new BufferGeometry().setFromPoints(pts), lineMat));
    }
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
        links(e.tasks.flatMap((t) => t.deps.map((dep) => [owner.get(dep), owner.get(t.id)])));
        break;
      case "task_started": if (d) mark("working"); break;
      case "drafted":
        if (d) {
          mark("review");
          // The reviewer walks over and stands beside the teammate's chair.
          reviewerTarget = d.stand.clone().addScaledVector(d.out, -0.5);
        }
        break;
      case "reviewed": if (d && e.unmet.length) mark("working"); break;
      case "task_accepted": if (d) { mark("accepted"); if (d.state === "accepted") d.hop = 1; } break;
      case "task_failed": if (d) mark("failed"); break;
      case "task_skipped": if (d) mark("skipped"); break;
      case "finished": case "run_failed": reviewerTarget = HOME.clone(); break;
    }
  }

  // Tags drawn before the web fonts arrived used a fallback font: redraw them once.
  document.fonts?.ready.then(() => {
    for (const d of desks.values()) setState(d, d.state);
    for (const [who, make] of [[lead, leadTag], [reviewer, reviewerTag]]) {
      dropTag(who.children.find((c) => c.isSprite));
      who.add(make());
    }
  });

  // Once a run is being watched, its events decide who is in the office; before
  // that, the team picked on the page sits at the desks.
  let watching = false;
  function showTeam(team) {
    if (watching) return;
    if (team) onEvent({ type: "staffed", team });
    else clearDesks();
  }
  addEventListener("agora:reset", () => { watching = true; clearDesks(); owner = new Map(); staffed = false; reviewerTarget = HOME.clone(); });
  addEventListener("agora:event", (ev) => { watching = true; onEvent(ev.detail); });
  addEventListener("agora:team", (ev) => showTeam(ev.detail));

  // Speech bubbles: whoever is talking in the chat says it over their head.
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
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false }));
    sprite.scale.set(3.0, 1.11, 1);
    sprite.position.y = 3.2;
    sprite.renderOrder = 2;
    return sprite;
  }
  const speaking = new Map(); // character → { sprite, until }
  function say(who, text, seconds) {
    if (!who) return;
    const old = speaking.get(who);
    if (old) dropTag(old.sprite);
    const sprite = bubble(text);
    who.add(sprite);
    speaking.set(who, { sprite, until: performance.now() + seconds * 1000, typing: text === "…" });
  }
  const speaker = (e) => (e.agent ? desks.get(e.agent)?.person : e.from === "Lead" || e.message?.kind === "lead" ? lead : null);
  addEventListener("agora:chat", ({ detail: e }) => {
    if (e.type === "typing") say(speaker(e), "…", 30);
    else if (e.type === "idle") for (const b of speaking.values()) { if (b.typing) b.until = 0; } // nobody is typing now
    else if (e.type === "message" && Date.now() - e.message.at_ms < 20000) say(speaker({ agent: e.message.agent, message: e.message }), e.message.text, 9);
  });

  // A slow orbit; drag to look around.
  const orbit = { angle: 0.75, distance: 17, height: 13, dragging: false, lastX: 0 };
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
    orbit.angle -= (e.clientX - orbit.lastX) * 0.008;
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

  const faceCamera = (c) => { c.rotation.y = Math.atan2(camera.position.x - c.position.x, camera.position.z - c.position.z); };

  // Sitting is a blend: 0 standing, 1 seated (legs forward).
  function pose(c, k) {
    c.userData.legs.forEach((l) => { l.rotation.x = -k * Math.PI / 2; });
    c.userData.shadow.position.y = 0.01 - c.position.y; // the shadow stays on the floor
  }
  pose(lead, 1);
  lead.userData.arms.forEach((a) => { a.rotation.x = -0.9; });

  const scratch = new Vector3();
  let last = performance.now();
  const loop = (now) => {
    const dt = Math.min((now - last) / 1000, 0.1);
    last = now;
    const t = now / 1000;
    if (!orbit.dragging) orbit.angle += dt * 0.04;
    if (!renderer.xr.isPresenting) {
      camera.position.set(Math.cos(orbit.angle) * orbit.distance, orbit.height, Math.sin(orbit.angle) * orbit.distance);
      camera.lookAt(0, 0.6, 0);
    }
    for (const d of desks.values()) {
      const p = d.person, u = p.userData;
      u.pose += ((SEATED.has(d.state) ? 1 : 0) - u.pose) * Math.min(1, dt * 6);
      // Slide between the spot beside the desk and the chair.
      p.position.lerpVectors(d.stand, d.seat, u.pose);
      if (d.hop > 0) { d.hop = Math.max(0, d.hop - dt * 1.4); p.position.y += Math.sin(d.hop * Math.PI) * 0.5; }
      if (u.pose > 0.5) p.lookAt(d.look.x, p.position.y, d.look.z); // at the desk, facing the monitor
      else faceCamera(p);
      pose(p, u.pose);
      const typing = d.state === "working";
      u.arms.forEach((a, k) => { a.rotation.x = typing ? -1.1 + Math.sin(t * 14 + k * Math.PI) * 0.25 : u.pose * -0.9; });
      u.body.position.y = Math.sin(t * 1.6 + p.position.x) * 0.012; // breathing
      u.head.rotation.z = d.state === "failed" ? 0.25 : 0; // a puzzled tilt
      u.head.rotation.x = typing ? Math.sin(t * 3) * 0.05 : 0;
    }
    lead.userData.head.rotation.x = Math.sin(t * 1.2) * 0.06; // the Lead nods over the plan

    const step = scratch.subVectors(reviewerTarget, reviewer.position).setY(0);
    const dist = step.length();
    const ru = reviewer.userData;
    if (dist > 0.05) {
      reviewer.position.addScaledVector(step.divideScalar(dist), Math.min(dist, dt * 4.5));
      reviewer.rotation.y = Math.atan2(step.x, step.z);
      const swing = Math.sin(t * 10) * 0.5;
      ru.legs[0].rotation.x = swing; ru.legs[1].rotation.x = -swing;
      ru.arms[0].rotation.x = -swing; ru.arms[1].rotation.x = swing;
      ru.body.position.y = Math.abs(Math.sin(t * 10)) * 0.04; // walking bob
    } else {
      faceCamera(reviewer);
      ru.legs.forEach((l) => { l.rotation.x = 0; });
      ru.arms.forEach((a) => { a.rotation.x = 0; });
      ru.body.position.y = 0;
    }
    for (const [who, b] of speaking) {
      if (now > b.until) { dropTag(b.sprite); speaking.delete(who); }
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
