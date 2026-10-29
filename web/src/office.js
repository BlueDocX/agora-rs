// The 3D office: one desk per task, a round little teammate at each desk showing
// what it is doing, and the purple reviewer walking to whichever desk has a draft.
//
// It only listens: the board page dispatches `agora:event` (one run event) and
// `agora:reset` (a run is about to replay from the start). Everything is procedural
// (white blocky furniture on a grid floor, toy-like characters), so there are no
// model files to load. Without WebGL2 the page keeps its kanban.

import {
  BoxGeometry, CanvasTexture, CapsuleGeometry, CylinderGeometry, ConeGeometry, DirectionalLight, GridHelper,
  Group, HemisphereLight, Line, LineBasicMaterial, BufferGeometry, Mesh, MeshLambertMaterial, PerspectiveCamera,
  PlaneGeometry, Scene, SphereGeometry, SRGBColorSpace, Sprite, SpriteMaterial, TorusGeometry, Vector3, WebGLRenderer,
} from "three";
import { VRButton } from "three/addons/webxr/VRButton.js";

const PALETTE = window.agoraPalette ?? ["#4285f4", "#34a853", "#f2b705", "#ea4335", "#14b8a6", "#f97316", "#ec4899", "#64748b"];
const STATE = {
  idle: ["#b3aa8f", "scheduled"], working: ["#4285f4", "working"], review: ["#d9af1b", "in review"],
  accepted: ["#1f9d55", "done"], failed: ["#e5483b", "needs a person"], skipped: ["#b3aa8f", "skipped"],
};
const HOME = new Vector3(0, 0, 2.2); // where the reviewer waits

// Started after the constants above exist (the bundle turns them into plain vars).
const host = document.getElementById("office3d");
const probe = document.createElement("canvas");
if (host && probe.getContext("webgl2")) start(host);

function start(host) {
  host.hidden = false;
  const badge = Object.assign(document.createElement("div"), { className: "badge", textContent: "WebGL2" });
  const hint = Object.assign(document.createElement("div"), { className: "hint", textContent: "Run a brief to staff the office" });
  host.append(badge, hint);

  const renderer = new WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.outputColorSpace = SRGBColorSpace;
  renderer.xr.enabled = true;
  host.prepend(renderer.domElement);
  // Only offer VR where it works (a headset browser), not a "not supported" button.
  navigator.xr?.isSessionSupported("immersive-vr").then((ok) => ok && host.append(VRButton.createButton(renderer)), () => {});

  const scene = new Scene();
  scene.add(new HemisphereLight(0xffffff, 0xd9d2bf, 2.3));
  const sun = new DirectionalLight(0xffffff, 1.5);
  sun.position.set(5, 11, 7);
  scene.add(sun);
  const camera = new PerspectiveCamera(36, 1, 0.1, 80);

  const mat = (color) => new MeshLambertMaterial({ color });
  const white = mat("#f4f1ea"), whiteShade = mat("#e7e2d6"), screenMat = mat("#2b2d31");

  // The floor: a white tiled slab on a blue rim.
  const SIZE = 17;
  const rim = new Mesh(new BoxGeometry(SIZE + 0.35, 0.28, SIZE + 0.35), mat("#3b82f6"));
  rim.position.y = -0.17;
  const slab = new Mesh(new BoxGeometry(SIZE, 0.3, SIZE), mat("#f7f5ef"));
  slab.position.y = -0.15;
  const grid = new GridHelper(SIZE, 34, 0xe3ddcf, 0xe3ddcf);
  grid.position.y = 0.002;
  scene.add(rim, slab, grid);

  // Furniture, all white and blocky. The ring of desks (radius ≤ 6.8) stays clear.
  const box = (w, h, d, x, y, z, m = white) => { const b = new Mesh(new BoxGeometry(w, h, d), m); b.position.set(x, y, z); scene.add(b); return b; };
  const table = new Mesh(new CylinderGeometry(1.1, 1.1, 0.12, 32), white);
  table.position.y = 0.72;
  const stem = new Mesh(new CylinderGeometry(0.16, 0.3, 0.7, 12), whiteShade);
  stem.position.y = 0.35;
  scene.add(table, stem);
  // A sofa in one corner, a whiteboard in another, low partitions, plants.
  box(2.6, 0.45, 0.9, -6.6, 0.23, 7.4); box(2.6, 0.8, 0.25, -6.6, 0.5, 7.95, whiteShade);
  box(0.25, 0.6, 0.9, -7.9, 0.4, 7.4, whiteShade); box(0.25, 0.6, 0.9, -5.3, 0.4, 7.4, whiteShade);
  const board = box(3, 1.7, 0.08, 6.4, 1.6, -8.05);
  board.material = mat("#fbfaf6");
  box(3.1, 0.1, 0.18, 6.4, 0.72, -7.98, whiteShade);
  box(0.08, 1.6, 0.08, 4.9, 0.8, -8.05, whiteShade); box(0.08, 1.6, 0.08, 7.9, 0.8, -8.05, whiteShade);
  box(0.25, 0.1, 0.05, 7.5, 0.82, -7.9, mat("#e5483b"));
  box(4.2, 1.0, 0.3, -6.1, 0.5, -7.9); box(0.3, 1.0, 3.2, -8.05, 0.5, -6.1);
  box(0.3, 0.9, 3.6, 8.05, 0.45, 5.6);
  for (const [x, z] of [[-7.6, 3.4], [7.4, 7.6], [2.6, -7.8]]) {
    const pot = new Mesh(new CylinderGeometry(0.3, 0.24, 0.5, 12), white);
    pot.position.set(x, 0.25, z);
    scene.add(pot);
    for (let i = 0; i < 5; i++) {
      const leaf = new Mesh(new ConeGeometry(0.1, 0.9, 4), mat("#4e9a52"));
      const a = (i / 5) * Math.PI * 2;
      leaf.position.set(x + Math.cos(a) * 0.12, 0.85, z + Math.sin(a) * 0.12);
      leaf.rotation.set(Math.sin(a) * 0.45, 0, -Math.cos(a) * 0.45);
      scene.add(leaf);
    }
  }

  // Shared pieces: every desk and character reuses these.
  const geo = {
    top: new BoxGeometry(1.7, 0.08, 0.85), side: new BoxGeometry(0.08, 0.7, 0.8), monitor: new BoxGeometry(0.75, 0.48, 0.06),
    screen: new PlaneGeometry(0.66, 0.39), stand: new BoxGeometry(0.06, 0.2, 0.06), keys: new BoxGeometry(0.5, 0.03, 0.16),
    head: new SphereGeometry(0.42, 28, 20), torso: new CapsuleGeometry(0.2, 0.18, 6, 14), limb: new CapsuleGeometry(0.075, 0.2, 4, 8),
    eye: new SphereGeometry(0.105, 16, 12), pupil: new SphereGeometry(0.058, 12, 10), smile: new TorusGeometry(0.06, 0.014, 6, 14, Math.PI),
    shadow: new PlaneGeometry(1.1, 1.1),
  };
  const shadowTex = (() => {
    const c = Object.assign(document.createElement("canvas"), { width: 64, height: 64 });
    const g = c.getContext("2d");
    const r = g.createRadialGradient(32, 32, 0, 32, 32, 32);
    r.addColorStop(0, "rgba(23,21,15,0.28)"); r.addColorStop(1, "rgba(23,21,15,0)");
    g.fillStyle = r; g.fillRect(0, 0, 64, 64);
    return new CanvasTexture(c);
  })();
  const eyeMat = mat("#ffffff"), pupilMat = mat("#1d1b16");
  const shadowMat = new MeshLambertMaterial({ map: shadowTex, transparent: true, depthWrite: false });
  const shared = new Set([...Object.values(geo), eyeMat, pupilMat, shadowMat, white, whiteShade, screenMat]);

  // A round toy-like teammate: big head, small body, big eyes, looking at the camera.
  function character(color) {
    const c = new Group();
    const skin = mat(color);
    const head = new Mesh(geo.head, skin); head.position.y = 1.12;
    const torso = new Mesh(geo.torso, skin); torso.position.y = 0.52;
    const legs = [-0.1, 0.1].map((x) => { const l = new Mesh(geo.limb, skin); l.position.set(x, 0.17, 0); return l; });
    const arms = [-1, 1].map((s) => { const a = new Mesh(geo.limb, skin); a.position.set(s * 0.25, 0.56, 0); a.rotation.z = s * 0.45; return a; });
    const face = new Group();
    for (const s of [-1, 1]) {
      const eye = new Mesh(geo.eye, eyeMat); eye.position.set(s * 0.15, 0, 0); eye.scale.z = 0.55;
      const pupil = new Mesh(geo.pupil, pupilMat); pupil.position.set(s * 0.15, -0.01, 0.045);
      face.add(eye, pupil);
    }
    const smile = new Mesh(geo.smile, pupilMat); smile.position.set(0, -0.16, -0.01); smile.rotation.z = Math.PI;
    face.add(smile);
    face.position.set(0, 1.18, 0.36);
    const shadow = new Mesh(geo.shadow, shadowMat); shadow.rotation.x = -Math.PI / 2; shadow.position.y = 0.01;
    const body = new Group();
    body.add(head, torso, ...legs, ...arms, face);
    c.add(shadow, body);
    Object.assign(c.userData, { body, arms, skin, color });
    return c;
  }

  // Name tag: the task's title, and a colored dot with what the teammate is doing.
  function tag(title, state) {
    const c = Object.assign(document.createElement("canvas"), { width: 512, height: 132 });
    const g = c.getContext("2d");
    g.fillStyle = "rgba(252,250,244,0.96)"; g.strokeStyle = "rgba(23,21,15,0.12)"; g.lineWidth = 3;
    g.beginPath(); g.roundRect(4, 4, 504, 124, 30); g.fill(); g.stroke();
    const chars = Array.from(title); // code points, so an emoji is never cut in half
    g.fillStyle = "#17150f"; g.font = "700 36px 'Schibsted Grotesk', system-ui, sans-serif"; g.textAlign = "center";
    g.fillText(chars.length > 24 ? chars.slice(0, 23).join("") + "…" : title, 256, 56);
    const [color, words] = STATE[state];
    g.font = "600 24px 'Spline Sans Mono', ui-monospace, monospace";
    const w = g.measureText(words.toUpperCase()).width;
    g.fillStyle = color; g.beginPath(); g.arc(256 - w / 2 - 14, 92, 8, 0, Math.PI * 2); g.fill();
    g.fillStyle = "#857d66"; g.textAlign = "left"; g.fillText(words.toUpperCase(), 256 - w / 2 + 4, 100);
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false }));
    sprite.scale.set(3.4, 0.88, 1);
    return sprite;
  }

  let desks = new Map(); // task id → { group, person, label, title, state, deps, hop }
  const deskLayer = new Group();
  scene.add(deskLayer);
  const reviewer = character("#6366c9");
  reviewer.position.copy(HOME);
  scene.add(reviewer);
  let reviewerTarget = HOME.clone();

  function dispose(root) {
    root.traverse((m) => {
      if (m.geometry && !m.isSprite && !shared.has(m.geometry)) m.geometry.dispose(); // sprites share three's own quad
      if (m.material && !shared.has(m.material)) { m.material.map?.dispose(); m.material.dispose(); }
    });
  }

  function clearDesks() {
    dispose(deskLayer);
    deskLayer.clear();
    desks = new Map();
    hint.hidden = false;
  }

  function setState(d, state) {
    d.state = state;
    d.label.parent.remove(d.label);
    dispose(d.label);
    d.label = tag(d.title, state);
    d.label.position.y = 2.4;
    d.person.add(d.label);
    const { skin, color } = d.person.userData;
    skin.color.set(state === "skipped" ? "#d9d3c4" : color); // skipped: greyed out
  }

  function plan(tasks) {
    clearDesks();
    hint.hidden = true;
    const n = tasks.length;
    const radius = Math.max(3.8, n * 0.85); // the planner caps plans at 8 tasks: at most 6.8, inside the room
    tasks.forEach((t, i) => {
      const angle = (i / n) * Math.PI * 2 - Math.PI / 2;
      const at = new Vector3(Math.cos(angle), 0, Math.sin(angle));
      // The desk faces the planning table; its teammate stands on the outside of it.
      const desk = new Group();
      const top = new Mesh(geo.top, white); top.position.y = 0.74;
      const sides = [-0.78, 0.78].map((x) => { const s = new Mesh(geo.side, whiteShade); s.position.set(x, 0.35, 0); return s; });
      const monitor = new Mesh(geo.monitor, white); monitor.position.set(0, 1.12, 0.12);
      const screen = new Mesh(geo.screen, screenMat); screen.position.set(0, 1.12, 0.084);
      screen.rotation.y = Math.PI;
      const stand = new Mesh(geo.stand, whiteShade); stand.position.set(0, 0.86, 0.12);
      const keys = new Mesh(geo.keys, whiteShade); keys.position.set(0, 0.795, -0.18);
      desk.add(top, ...sides, monitor, screen, stand, keys);
      desk.position.copy(at).multiplyScalar(radius);
      desk.lookAt(0, 0, 0);
      deskLayer.add(desk);

      const person = character(PALETTE[i % PALETTE.length]);
      person.position.copy(at).multiplyScalar(radius + 0.85);
      deskLayer.add(person);
      const d = { desk, person, label: new Group(), title: t.title, state: "idle", deps: t.deps, hop: 0 };
      person.add(d.label);
      setState(d, "idle");
      desks.set(t.id, d);
    });
    // Faint blue lines for "this task needs that one's output".
    const lineMat = new LineBasicMaterial({ color: 0x8fb3f5 });
    for (const d of desks.values()) {
      for (const dep of d.deps) {
        const from = desks.get(dep);
        if (!from) continue;
        const pts = [from.desk.position.clone().setY(0.03), d.desk.position.clone().setY(0.03)];
        deskLayer.add(new Line(new BufferGeometry().setFromPoints(pts), lineMat));
      }
    }
    orbit.distance = radius * 1.55 + 6;
    orbit.height = radius * 1.15 + 6;
  }

  function onEvent(e) {
    const d = e.task && desks.get(e.task);
    switch (e.type) {
      case "planned": plan(e.tasks); break;
      case "task_started": if (d) setState(d, "working"); break;
      case "drafted":
        if (d) {
          setState(d, "review");
          // The reviewer walks over and stands beside the desk.
          reviewerTarget = d.desk.position.clone().multiplyScalar(0.7);
        }
        break;
      case "reviewed": if (d && e.unmet.length) setState(d, "working"); break;
      case "task_accepted": if (d) { setState(d, "accepted"); d.hop = 1; } break;
      case "task_failed": if (d) setState(d, "failed"); break;
      case "task_skipped": if (d) setState(d, "skipped"); break;
      case "finished": case "run_failed": reviewerTarget = HOME.clone(); break;
    }
  }

  // Tags drawn before the web fonts arrived used a fallback font: redraw them once.
  document.fonts?.ready.then(() => { for (const d of desks.values()) setState(d, d.state); });

  addEventListener("agora:reset", () => { clearDesks(); reviewerTarget = HOME.clone(); });
  addEventListener("agora:event", (ev) => onEvent(ev.detail));

  // A slow orbit; drag to look around.
  const orbit = { angle: 0.75, distance: 14, height: 11, dragging: false, lastX: 0 };
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
      const p = d.person, { body, arms } = p.userData;
      faceCamera(p);
      const busy = d.state === "working";
      body.position.y = busy ? Math.abs(Math.sin(t * 6)) * 0.05 : Math.sin(t * 1.6 + p.position.x) * 0.012; // typing, or breathing
      arms.forEach((a, k) => { a.rotation.x = busy ? Math.sin(t * 12 + k * Math.PI) * 0.5 : 0; });
      body.rotation.z = d.state === "failed" ? 0.22 : 0; // a puzzled tilt
      if (d.hop > 0) { d.hop = Math.max(0, d.hop - dt * 1.4); body.position.y = Math.sin(d.hop * Math.PI) * 0.5; }
    }
    const step = reviewerTarget.clone().sub(reviewer.position).setY(0);
    const dist = step.length();
    const rb = reviewer.userData.body;
    if (dist > 0.05) {
      reviewer.position.addScaledVector(step.divideScalar(dist), Math.min(dist, dt * 4.5));
      reviewer.rotation.y = Math.atan2(step.x, step.z);
      rb.position.y = Math.abs(Math.sin(t * 10)) * 0.07; // walking bob
    } else {
      faceCamera(reviewer);
      rb.position.y = 0;
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
  for (const e of window.agoraHistory?.() ?? []) onEvent(e);
}
