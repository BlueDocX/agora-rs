// The 3D office: one desk per task, an avatar at each desk showing what that
// teammate is doing, and the reviewer walking to whichever desk has a draft to check.
//
// It only listens: the board page dispatches `agora:event` (one run event) and
// `agora:reset` (a run is about to replay from the start). Everything is procedural
// and flat-shaded, so there are no model files to load; the whole scene stays well
// under 100 draw calls. Without WebGL2 the page keeps its 2D cards.

import {
  BoxGeometry, CanvasTexture, CapsuleGeometry, CircleGeometry, Color, ConeGeometry, CylinderGeometry,
  DirectionalLight, Fog, Group, HemisphereLight, IcosahedronGeometry, Line, LineBasicMaterial,
  BufferGeometry, Mesh, MeshLambertMaterial, PerspectiveCamera, Scene, SphereGeometry, SRGBColorSpace,
  Sprite, SpriteMaterial, Vector3, WebGLRenderer,
} from "three";
import { VRButton } from "three/addons/webxr/VRButton.js";

const host = document.getElementById("office3d");
const probe = document.createElement("canvas");
if (host && probe.getContext("webgl2")) start(host);

function css(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
}

function start(host) {
  host.hidden = false;
  const palette = {
    floor: new Color(css("--bg")), idle: new Color(css("--idle")), working: new Color(css("--accent")),
    review: new Color(css("--warn")), accepted: new Color(css("--ok")), failed: new Color(css("--bad")),
    reviewer: new Color("#8a5cf6"), wood: new Color("#c9a27a"), skin: new Color("#f1d3b8"),
  };

  const renderer = new WebGLRenderer({ antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.outputColorSpace = SRGBColorSpace;
  renderer.xr.enabled = true;
  host.append(renderer.domElement);
  // Only offer VR where it works (a headset browser), not a "not supported" button.
  navigator.xr?.isSessionSupported("immersive-vr").then((ok) => ok && host.append(VRButton.createButton(renderer)), () => {});

  const scene = new Scene();
  scene.fog = new Fog(palette.floor, 14, 30);
  scene.add(new HemisphereLight(0xffffff, 0x8a8a90, 2.2));
  const sun = new DirectionalLight(0xffffff, 1.4);
  sun.position.set(4, 10, 6);
  scene.add(sun);

  const camera = new PerspectiveCamera(42, 1, 0.1, 60);
  const mat = (color) => new MeshLambertMaterial({ color, flatShading: true });

  // The room: a round floor, a planning table in the middle, a few plants.
  const floor = new Mesh(new CircleGeometry(10, 48), mat(palette.floor.clone().offsetHSL(0, 0, -0.04)));
  floor.rotation.x = -Math.PI / 2;
  scene.add(floor);
  const table = new Mesh(new CylinderGeometry(1.1, 1.2, 0.75, 8), mat(palette.wood));
  table.position.y = 0.375;
  scene.add(table);
  for (const a of [0.6, 2.2, 3.8, 5.4]) {
    const plant = new Group();
    const pot = new Mesh(new CylinderGeometry(0.28, 0.22, 0.4, 6), mat("#b5651d"));
    pot.position.y = 0.2;
    const leaves = new Mesh(new ConeGeometry(0.45, 1.1, 5), mat("#3f8f4f"));
    leaves.position.y = 0.95;
    plant.add(pot, leaves);
    plant.position.set(Math.cos(a) * 8.2, 0, Math.sin(a) * 8.2);
    scene.add(plant);
  }

  // Shared geometry: every desk and avatar reuses these.
  const geo = {
    top: new BoxGeometry(1.6, 0.08, 0.8), leg: new BoxGeometry(1.4, 0.7, 0.6), screen: new BoxGeometry(0.7, 0.45, 0.05),
    body: new CapsuleGeometry(0.28, 0.5, 4, 8), head: new SphereGeometry(0.26, 12, 8), orb: new IcosahedronGeometry(0.13, 0),
  };

  function label(text) {
    const c = document.createElement("canvas");
    c.width = 512; c.height = 96;
    const g = c.getContext("2d");
    g.fillStyle = "rgba(255,255,255,0.92)";
    g.beginPath(); g.roundRect(4, 4, 504, 88, 20); g.fill();
    g.fillStyle = "#1d1d1f";
    g.font = "600 34px system-ui, sans-serif";
    g.textAlign = "center"; g.textBaseline = "middle";
    const chars = Array.from(text); // code points, so an emoji is never cut in half
    const t = chars.length > 26 ? chars.slice(0, 25).join("") + "…" : text;
    g.fillText(t, 256, 50);
    const map = new CanvasTexture(c);
    map.colorSpace = SRGBColorSpace;
    const sprite = new Sprite(new SpriteMaterial({ map, depthWrite: false }));
    sprite.scale.set(3.2, 0.6, 1);
    return sprite;
  }

  function avatar(color) {
    const a = new Group();
    const body = new Mesh(geo.body, mat(color));
    body.position.y = 0.75;
    const head = new Mesh(geo.head, mat(palette.skin));
    head.position.y = 1.42;
    a.add(body, head);
    a.userData.body = body;
    return a;
  }

  let desks = new Map(); // task id → { group, person, orb, state, base }
  const deskLayer = new Group();
  scene.add(deskLayer);
  const reviewer = avatar(palette.reviewer);
  reviewer.position.set(0, 0, 1.6);
  scene.add(reviewer);
  let reviewerTarget = new Vector3(0, 0, 1.6);

  const shared = new Set(Object.values(geo));
  function clearDesks() {
    deskLayer.traverse((m) => {
      if (m.geometry && !shared.has(m.geometry)) m.geometry.dispose();
      if (m.material) { m.material.map?.dispose(); m.material.dispose(); }
    });
    deskLayer.clear();
    desks = new Map();
  }

  function setState(desk, state) {
    desk.state = state;
    const color = palette[state === "skipped" ? "idle" : state] || palette.idle;
    desk.person.userData.body.material.color.copy(color);
    desk.orb.material.color.copy(color);
    desk.group.traverse((m) => {
      if (m.material && !m.isSprite) { m.material.transparent = state === "skipped"; m.material.opacity = state === "skipped" ? 0.35 : 1; }
    });
  }

  function plan(tasks) {
    clearDesks();
    const n = tasks.length;
    const radius = Math.max(3.6, n * 0.85); // the planner caps plans at 8 tasks: at most 6.8, inside the plants
    tasks.forEach((t, i) => {
      const angle = (i / n) * Math.PI * 2 - Math.PI / 2;
      const group = new Group();
      const top = new Mesh(geo.top, mat(palette.wood)); top.position.y = 0.74;
      const leg = new Mesh(geo.leg, mat(palette.wood.clone().offsetHSL(0, 0, -0.12))); leg.position.y = 0.35;
      const screen = new Mesh(geo.screen, mat("#2b2d31")); screen.position.set(0, 1.05, -0.2);
      const person = avatar(palette.idle); person.position.set(0, 0, -0.75);
      const orb = new Mesh(geo.orb, mat(palette.idle)); orb.position.set(0, 2.05, -0.75);
      const tag = label(t.title); tag.position.set(0, 2.7, -0.75);
      group.add(top, leg, screen, person, orb, tag);
      // Each teammate sits on the outside of the ring, facing the planning table.
      group.position.set(Math.cos(angle) * radius, 0, Math.sin(angle) * radius);
      group.lookAt(0, 0, 0);
      group.rotateY(Math.PI);
      deskLayer.add(group);
      desks.set(t.id, { group, person, orb, state: "idle", deps: t.deps });
    });
    // Faint lines for "this task needs that one's output".
    for (const [id, d] of desks) {
      for (const dep of d.deps) {
        const from = desks.get(dep);
        if (!from) continue;
        const pts = [from.group.position.clone().setY(0.02), d.group.position.clone().setY(0.02)];
        deskLayer.add(new Line(new BufferGeometry().setFromPoints(pts), new LineBasicMaterial({ color: palette.idle })));
      }
    }
    frame(radius);
  }

  function frame(radius) {
    orbit.distance = radius * 1.7 + 3.5;
    orbit.height = radius * 1.1 + 2.5;
  }

  function onEvent(e) {
    const d = e.task && desks.get(e.task);
    switch (e.type) {
      case "planned": plan(e.tasks); break;
      case "task_started": if (d) setState(d, "working"); break;
      case "drafted":
        if (d) {
          setState(d, "review");
          // The reviewer walks over and stands in front of the desk.
          reviewerTarget = d.group.position.clone().multiplyScalar(0.72);
        }
        break;
      case "reviewed": if (d && e.unmet.length) setState(d, "working"); break;
      case "task_accepted": if (d) { setState(d, "accepted"); d.hop = 1; } break;
      case "task_failed": if (d) setState(d, "failed"); break;
      case "task_skipped": if (d) setState(d, "skipped"); break;
      case "finished": case "run_failed": reviewerTarget = new Vector3(0, 0, 1.6); break;
    }
  }

  addEventListener("agora:reset", () => { clearDesks(); reviewerTarget = new Vector3(0, 0, 1.6); });
  addEventListener("agora:event", (ev) => onEvent(ev.detail));

  // A slow orbit; drag to look around.
  const orbit = { angle: 0.6, distance: 12, height: 7, dragging: false, lastX: 0 };
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
    const w = host.clientWidth, h = Math.max(260, Math.round(w * 0.5));
    renderer.setSize(w, h, false);
    renderer.domElement.style.width = "100%";
    renderer.domElement.style.height = h + "px";
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
  new ResizeObserver(resize).observe(host);
  resize();

  let last = performance.now();
  const loop = (now) => {
    const dt = Math.min((now - last) / 1000, 0.1);
    last = now;
    const t = now / 1000;
    if (!orbit.dragging) orbit.angle += dt * 0.05;
    if (!renderer.xr.isPresenting) {
      camera.position.set(Math.cos(orbit.angle) * orbit.distance, orbit.height, Math.sin(orbit.angle) * orbit.distance);
      camera.lookAt(0, 0.8, 0);
    }
    for (const d of desks.values()) {
      const p = d.person;
      p.position.y = d.state === "working" ? Math.abs(Math.sin(t * 6)) * 0.05 : 0; // typing
      if (d.hop > 0) { d.hop = Math.max(0, d.hop - dt * 1.5); p.position.y = Math.sin(d.hop * Math.PI) * 0.45; }
      d.orb.rotation.y += dt * (d.state === "review" ? 4 : 0.8);
    }
    const step = reviewerTarget.clone().sub(reviewer.position).setY(0);
    const dist = step.length();
    if (dist > 0.05) {
      reviewer.position.addScaledVector(step.divideScalar(dist), Math.min(dist, dt * 4.5));
      reviewer.lookAt(reviewerTarget.x, 0, reviewerTarget.z);
      reviewer.position.y = Math.abs(Math.sin(t * 10)) * 0.06; // walking bob
    } else {
      reviewer.position.y = 0;
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
