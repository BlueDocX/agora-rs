"""Builds the office kit and the teammate character in Blender and exports them as glTF.

    blender -b -P assets/blender/build_assets.py -- crates/agora-server/static/models [preview_dir]
    (it writes each .glb and its .glb.gz, which the server sends to browsers that accept gzip)

Everything is made from primitives here, so the models are reproducible and ours.
Conventions the page relies on (web/src/office.js):
- Each piece is one top-level node, named (Desk, Chair, RoundTable, ...), origin on the floor.
- Front faces +Z in glTF (Blender -Y): where you sit, what a character looks at.
- Materials named Skin / Accent / Screen are recolored per teammate in the browser.
- The character's parts are separate nodes (Head, Body, ArmL, ...) so the page can
  animate them without a skeleton.
"""

import gzip
import math
import sys
from pathlib import Path

import bpy
from mathutils import Vector

args = sys.argv[sys.argv.index("--") + 1 :] if "--" in sys.argv else []
OUT = Path(args[0] if args else ".").resolve()
PREVIEW = Path(args[1]).resolve() if len(args) > 1 else None

# --- helpers -------------------------------------------------------------------

_mats = {}


def material(name, rgb, rough=0.6, emit=None):
    """One shared material per name."""
    if name in _mats:
        return _mats[name]
    m = bpy.data.materials.new(name)
    if m.node_tree is None:
        m.use_nodes = True
    bsdf = next(n for n in m.node_tree.nodes if n.type == "BSDF_PRINCIPLED")
    rgba = (*[(c / 255) ** 2.2 for c in rgb], 1.0)  # sRGB → linear
    bsdf.inputs["Base Color"].default_value = rgba
    bsdf.inputs["Roughness"].default_value = rough
    if emit:
        bsdf.inputs["Emission Color"].default_value = (*[(c / 255) ** 2.2 for c in emit], 1.0)
        bsdf.inputs["Emission Strength"].default_value = 1.0
    m.diffuse_color = rgba
    _mats[name] = m
    return m


WHITE = (244, 241, 234)
SHADE = (224, 218, 204)
WOOD = (214, 184, 148)
INK = (34, 32, 28)


def adopt(obj, parent):
    """Parent without moving: the object keeps its world position."""
    bpy.context.view_layer.update()
    obj.parent = parent
    obj.matrix_parent_inverse = parent.matrix_world.inverted()


def finish(obj, mat, parent, bevel=0.0, smooth=False):
    obj.data.materials.append(material(*mat) if isinstance(mat, tuple) else mat)
    if bevel:
        mod = obj.modifiers.new("bevel", "BEVEL")
        mod.width, mod.segments, mod.limit_method = bevel, 2, "ANGLE"
    if smooth:
        for p in obj.data.polygons:
            p.use_smooth = True
    adopt(obj, parent)
    return obj


def box(parent, mat, size, at, bevel=0.015, rot=(0, 0, 0), name=None):
    bpy.ops.mesh.primitive_cube_add(size=1, location=at, rotation=rot)
    o = bpy.context.object
    o.scale = size
    bpy.ops.object.transform_apply(scale=True)
    if name:
        o.name = name
    return finish(o, mat, parent, bevel=bevel)


def cyl(parent, mat, r, depth, at, verts=24, rot=(0, 0, 0), bevel=0.0, r2=None, name=None):
    if r2 is None:
        bpy.ops.mesh.primitive_cylinder_add(vertices=verts, radius=r, depth=depth, location=at, rotation=rot)
    else:
        bpy.ops.mesh.primitive_cone_add(vertices=verts, radius1=r, radius2=r2, depth=depth, location=at, rotation=rot)
    o = bpy.context.object
    if name:
        o.name = name
    return finish(o, mat, parent, bevel=bevel, smooth=True)


def ball(parent, mat, r, at, scale=(1, 1, 1), seg=24, rings=16, rot=(0, 0, 0), name=None):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=seg, ring_count=rings, radius=r, location=at, rotation=rot)
    o = bpy.context.object
    o.scale = scale
    bpy.ops.object.transform_apply(scale=True)
    if name:
        o.name = name
    return finish(o, mat, parent, smooth=True)


def torus(parent, mat, major, minor, at, rot=(0, 0, 0), name=None):
    bpy.ops.mesh.primitive_torus_add(major_radius=major, minor_radius=minor, major_segments=32, minor_segments=10, location=at, rotation=rot)
    o = bpy.context.object
    if name:
        o.name = name
    return finish(o, mat, parent, smooth=True)


def root(name, x=0.0, y=0.0):
    """An empty at the piece's floor origin; built apart so pieces don't overlap in previews."""
    bpy.ops.object.empty_add(location=(x, y, 0))
    e = bpy.context.object
    e.name = name
    return e


def at(r, x, y, z):
    return (r.location.x + x, r.location.y + y, z)


# --- the office kit ---------------------------------------------------------------


def desk(x, y=0.0):
    r = root("Desk", x, y)
    box(r, ("White", WHITE), (1.6, 0.8, 0.05), at(r, 0, 0, 0.735), bevel=0.02)
    for s in (-1, 1):
        box(r, ("Shade", SHADE), (0.05, 0.72, 0.71), at(r, s * 0.74, 0, 0.355))
    box(r, ("Shade", SHADE), (1.43, 0.03, 0.4), at(r, 0, 0.3, 0.5))  # modesty panel
    # Monitor at the back, facing the sitter (-Y).
    box(r, ("Ink", INK), (0.74, 0.035, 0.44), at(r, 0, 0.2, 1.12), bevel=0.012)
    box(r, material("Screen", (60, 64, 80), rough=0.3, emit=(25, 28, 40)), (0.68, 0.01, 0.38), at(r, 0, 0.18, 1.12), bevel=0, name="DeskScreen")
    box(r, ("Shade", SHADE), (0.05, 0.05, 0.3), at(r, 0, 0.23, 0.9))
    box(r, ("Shade", SHADE), (0.26, 0.18, 0.015), at(r, 0, 0.23, 0.765))
    box(r, ("White", WHITE), (0.46, 0.15, 0.018), at(r, 0, -0.12, 0.77), bevel=0.006)  # keyboard
    ball(r, ("White", WHITE), 0.04, at(r, 0.36, -0.12, 0.77), scale=(0.8, 1.2, 0.5), seg=12, rings=8)  # mouse
    cyl(r, ("Accent", (242, 204, 70)), 0.04, 0.1, at(r, -0.6, 0.05, 0.81), verts=16)  # mug
    # A little plant at the corner.
    cyl(r, ("White", WHITE), 0.06, 0.1, at(r, 0.66, 0.25, 0.81), verts=16)
    for i in range(4):
        a = i * math.pi / 2
        ball(r, ("Leaf", (78, 154, 82)), 0.05, at(r, 0.66 + 0.03 * math.cos(a), 0.25 + 0.03 * math.sin(a), 0.89), scale=(0.6, 0.6, 1.5), seg=10, rings=6, rot=(0.4 * math.sin(a), -0.4 * math.cos(a), 0))
    return r


def chair(x, y=0.0):
    r = root("Chair", x, y)
    for i in range(5):  # star base with casters
        a = i * 2 * math.pi / 5
        box(r, ("Ink", INK), (0.3, 0.05, 0.035), at(r, 0.15 * math.cos(a), 0.15 * math.sin(a), 0.07), rot=(0, 0, a), bevel=0.01)
        ball(r, ("Ink", INK), 0.03, at(r, 0.29 * math.cos(a), 0.29 * math.sin(a), 0.03), seg=10, rings=6)
    cyl(r, ("Ink", INK), 0.028, 0.32, at(r, 0, 0, 0.25), verts=12)
    box(r, ("Accent", (242, 204, 70)), (0.5, 0.48, 0.09), at(r, 0, 0, 0.46), bevel=0.04)  # seat
    box(r, ("Accent", (242, 204, 70)), (0.46, 0.08, 0.48), at(r, 0, 0.25, 0.8), rot=(-0.12, 0, 0), bevel=0.04)  # back (+Y = behind)
    box(r, ("Ink", INK), (0.05, 0.04, 0.3), at(r, 0, 0.27, 0.56))
    for s in (-1, 1):
        box(r, ("Shade", SHADE), (0.05, 0.3, 0.035), at(r, s * 0.27, 0.0, 0.62), bevel=0.012)
        box(r, ("Shade", SHADE), (0.035, 0.035, 0.15), at(r, s * 0.27, 0.05, 0.54))
    return r


def round_table(x, y=0.0):
    r = root("RoundTable", x, y)
    cyl(r, ("White", WHITE), 1.15, 0.07, at(r, 0, 0, 0.74), verts=48, bevel=0.02)
    cyl(r, ("Shade", SHADE), 0.12, 0.68, at(r, 0, 0, 0.37), verts=20)
    cyl(r, ("Shade", SHADE), 0.45, 0.04, at(r, 0, 0, 0.02), verts=32, r2=0.38)
    for i in range(3):  # sticky notes and a laptop on the table
        box(r, ("Accent", (242, 204, 70)), (0.12, 0.12, 0.005), at(r, -0.4 + 0.18 * i, -0.3 + 0.07 * i, 0.78), rot=(0, 0, 0.2 * i), bevel=0)
    box(r, ("Shade", SHADE), (0.36, 0.25, 0.015), at(r, 0.35, 0.2, 0.785), bevel=0.006)
    box(r, ("Shade", SHADE), (0.36, 0.015, 0.23), at(r, 0.35, 0.33, 0.9), rot=(-0.25, 0, 0), bevel=0.006)
    return r


def sofa(x, y=0.0):
    r = root("Sofa", x, y)
    box(r, ("Fabric", (226, 220, 206)), (2.2, 0.85, 0.42), at(r, 0, 0, 0.23), bevel=0.06)
    for i in (-1, 1):
        box(r, ("Cushion", (236, 231, 219)), (1.0, 0.7, 0.14), at(r, i * 0.53, -0.05, 0.5), bevel=0.06)
        box(r, ("Cushion", (236, 231, 219)), (1.0, 0.18, 0.5), at(r, i * 0.53, 0.33, 0.72), rot=(-0.15, 0, 0), bevel=0.07)
        box(r, ("Fabric", (226, 220, 206)), (0.2, 0.85, 0.62), at(r, i * 1.1, 0, 0.33), bevel=0.07)
    box(r, ("Accent", (242, 204, 70)), (0.38, 0.14, 0.34), at(r, -0.75, 0.2, 0.7), rot=(-0.3, 0, 0.2), bevel=0.06)  # pillow
    return r


def coffee_table(x, y=0.0):
    r = root("CoffeeTable", x, y)
    cyl(r, ("Rug", (232, 225, 208)), 1.3, 0.012, at(r, 0, 0, 0.006), verts=48)
    cyl(r, ("Wood", WOOD), 0.45, 0.05, at(r, 0, 0, 0.4), verts=32, bevel=0.015)
    for i in range(3):
        a = i * 2 * math.pi / 3
        cyl(r, ("Wood", WOOD), 0.025, 0.38, at(r, 0.3 * math.cos(a), 0.3 * math.sin(a), 0.19), verts=10)
    box(r, ("Accent", (242, 204, 70)), (0.22, 0.16, 0.04), at(r, 0.05, 0, 0.445), rot=(0, 0, 0.4))  # a book
    return r


def plant(x, y=0.0):
    r = root("Plant", x, y)
    cyl(r, ("White", WHITE), 0.26, 0.5, at(r, 0, 0, 0.25), verts=24, r2=0.2, bevel=0.01)
    cyl(r, ("Soil", (110, 84, 60)), 0.24, 0.02, at(r, 0, 0, 0.5), verts=24)
    for i in range(9):
        a = i * 2 * math.pi / 9
        tilt = 0.35 + 0.15 * (i % 3)
        h = 0.55 + 0.12 * (i % 2)
        ball(r, ("Leaf", (78, 154, 82)), 0.09, at(r, 0.12 * math.cos(a), 0.12 * math.sin(a), 0.52 + h / 2), scale=(0.7, 0.35, h / 0.18),
             seg=12, rings=8, rot=(tilt * math.sin(a), -tilt * math.cos(a), a))
    return r


def whiteboard(x, y=0.0):
    r = root("Whiteboard", x, y)
    box(r, ("Board", (251, 250, 246)), (1.8, 0.04, 1.05), at(r, 0, 0, 1.35), bevel=0.01)
    box(r, ("Shade", SHADE), (1.86, 0.05, 0.05), at(r, 0, 0, 1.9))
    box(r, ("Shade", SHADE), (1.86, 0.09, 0.04), at(r, 0, -0.03, 0.81))
    for s in (-1, 1):
        box(r, ("Shade", SHADE), (0.04, 0.04, 1.9), at(r, s * 0.95, 0, 0.95))
        box(r, ("Shade", SHADE), (0.04, 0.5, 0.04), at(r, s * 0.95, 0, 0.04))
    # A few scribbles: the "plan" on the board.
    for i, (w, c) in enumerate([(0.9, (66, 133, 244)), (0.6, (234, 67, 53)), (0.75, (52, 168, 83)), (0.4, (34, 32, 28))]):
        box(r, ("Marker" + str(i), c), (w, 0.005, 0.03), at(r, -0.35 + w / 2 - 0.2, -0.025, 1.65 - i * 0.16), bevel=0)
    return r


def shelf(x, y=0.0):
    r = root("Shelf", x, y)
    box(r, ("White", WHITE), (1.4, 0.36, 0.04), at(r, 0, 0, 1.6))
    for s in (-1, 1):
        box(r, ("White", WHITE), (0.04, 0.36, 1.6), at(r, s * 0.68, 0, 0.8))
    colors = [(66, 133, 244), (242, 204, 70), (52, 168, 83), (234, 67, 53), (99, 102, 201), (226, 220, 206)]
    for level in range(3):
        z = 0.05 + level * 0.52
        box(r, ("White", WHITE), (1.36, 0.34, 0.03), at(r, 0, 0, z))
        xpos = -0.6
        for k in range(7 - level):
            w = 0.07 + 0.02 * ((k + level) % 3)
            h = 0.3 + 0.05 * ((k * 7 + level) % 3)
            c = colors[(k + level * 2) % len(colors)]
            box(r, ("Book" + str(colors.index(c)), c), (w, 0.26, h), at(r, xpos + w / 2, 0, z + 0.015 + h / 2), bevel=0.005)
            xpos += w + 0.015
    return r


def lamp(x, y=0.0):
    r = root("Lamp", x, y)
    cyl(r, ("Ink", INK), 0.18, 0.03, at(r, 0, 0, 0.015), verts=24)
    cyl(r, ("Ink", INK), 0.015, 1.5, at(r, 0, 0, 0.77), verts=10)
    cyl(r, material("LampShade", (250, 240, 214), emit=(255, 228, 170)), 0.25, 0.3, at(r, 0, 0, 1.55), verts=24, r2=0.14)
    return r


def cooler(x, y=0.0):
    r = root("Cooler", x, y)
    box(r, ("White", WHITE), (0.4, 0.4, 1.0), at(r, 0, 0, 0.5), bevel=0.03)
    cyl(r, material("Water", (150, 196, 245), rough=0.1), 0.17, 0.45, at(r, 0, 0, 1.23), verts=24, bevel=0.03)
    box(r, ("Ink", INK), (0.08, 0.05, 0.05), at(r, 0, -0.21, 0.82))
    return r


# --- the teammate -------------------------------------------------------------------


def chibi(x, y=0.0):
    """A round toy-like teammate, about 1.45 tall: big head, short body, headset."""
    r = root("Chibi", x, y)
    skin = material("Skin", (66, 133, 244), rough=0.45)
    white = material("EyeWhite", (255, 255, 255), rough=0.2)
    dark = material("Pupil", (29, 27, 22), rough=0.25)
    blush = material("Blush", (255, 150, 160), rough=0.6)
    gear = material("Gear", (40, 40, 46), rough=0.4)

    def part(name, z, dx=0.0):
        """A pivot (neck, shoulder, hip) the page rotates to animate that part."""
        bpy.ops.object.empty_add(location=at(r, dx, 0, z))
        p = bpy.context.object
        p.name = name
        adopt(p, r)
        return p

    body = part("Body", 0.0)
    ball(body, skin, 0.25, at(r, 0, 0, 0.55), scale=(1.0, 0.9, 1.1))  # torso
    ball(body, material("Belly", (255, 255, 255), rough=0.5), 0.14, at(r, 0, -0.17, 0.52), scale=(1.0, 0.4, 1.05), seg=16, rings=10)
    head = part("Head", 0.82)
    ball(head, skin, 0.44, at(r, 0, 0, 1.08), scale=(1.05, 0.97, 0.94), seg=32, rings=20)
    for s in (-1, 1):
        ball(head, white, 0.105, at(r, s * 0.16, -0.36, 1.1), scale=(0.85, 0.5, 1.15), seg=18, rings=12)
        ball(head, dark, 0.07, at(r, s * 0.16, -0.4, 1.09), scale=(0.85, 0.5, 1.15), seg=14, rings=10)
        ball(head, white, 0.022, at(r, s * 0.16 + 0.025, -0.43, 1.13), seg=8, rings=6)  # sparkle
        ball(head, blush, 0.06, at(r, s * 0.28, -0.32, 0.98), scale=(1.0, 0.3, 0.6), seg=12, rings=8)
        # Headset: ear cups on each side.
        cyl(head, gear, 0.09, 0.07, at(r, s * 0.45, 0, 1.08), verts=20, rot=(0, math.pi / 2, 0), bevel=0.02)
    torus(head, dark, 0.05, 0.012, at(r, 0, -0.39, 0.97), rot=(math.pi / 2 + 0.25, 0, 0))  # smile
    band = torus(head, gear, 0.46, 0.025, at(r, 0, 0, 1.1), rot=(math.pi / 2, 0, 0))  # ear to ear
    # Keep only the top half of the band.
    bpy.context.view_layer.objects.active = band
    bpy.ops.object.mode_set(mode="EDIT")
    bpy.ops.mesh.select_all(action="DESELECT")
    bpy.ops.object.mode_set(mode="OBJECT")
    for v in band.data.vertices:
        v.select = (band.matrix_world @ v.co).z < 1.1
    bpy.ops.object.mode_set(mode="EDIT")
    bpy.ops.mesh.delete(type="VERT")
    bpy.ops.object.mode_set(mode="OBJECT")
    cyl(head, gear, 0.012, 0.18, at(r, -0.42, -0.12, 0.98), verts=8, rot=(1.3, 0, -0.5))  # mic boom
    ball(head, gear, 0.025, at(r, -0.36, -0.25, 0.94), seg=10, rings=6)
    ball(head, skin, 0.07, at(r, 0.02, 0.02, 1.53), scale=(0.7, 0.7, 1.2), seg=12, rings=8, rot=(0, 0.35, 0))  # a little tuft
    for s, name in ((-1, "ArmR"), (1, "ArmL")):
        arm = part(name, 0.68, s * 0.24)
        ball(arm, skin, 0.075, at(r, s * 0.29, 0, 0.56), scale=(0.9, 0.9, 2.0), seg=14, rings=10, rot=(0, s * 0.35, 0))
    for s, name in ((-1, "LegR"), (1, "LegL")):
        leg = part(name, 0.32, s * 0.11)
        ball(leg, skin, 0.085, at(r, s * 0.11, 0, 0.2), scale=(1.0, 1.0, 1.6), seg=14, rings=10)
        ball(leg, skin, 0.09, at(r, s * 0.11, -0.04, 0.06), scale=(1.0, 1.35, 0.65), seg=14, rings=10)  # foot
    return r


# --- build, preview, export --------------------------------------------------------------


def reset():
    bpy.ops.wm.read_factory_settings(use_empty=True)
    _mats.clear()


def preview(path, target, dist):
    scene = bpy.context.scene
    scene.render.engine = "BLENDER_WORKBENCH"
    scene.display.shading.light = "STUDIO"
    scene.display.shading.color_type = "MATERIAL"
    scene.display.shading.show_shadows = True
    scene.display.shading.show_cavity = True
    scene.render.resolution_x, scene.render.resolution_y = 1400, 700
    scene.render.film_transparent = False
    bpy.ops.object.camera_add(location=(target[0] + dist * 0.55, target[1] - dist, target[2] + dist * 0.45))
    cam = bpy.context.object
    direction = Vector(target) - cam.location
    cam.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()
    cam.data.lens = 50
    scene.camera = cam
    scene.render.filepath = str(path)
    bpy.ops.render.render(write_still=True)
    bpy.data.objects.remove(cam)


def export(path, roots):
    bpy.ops.object.select_all(action="DESELECT")
    for r in roots:
        r.location.x = r.location.y = 0  # each piece at the origin in the file; the page places clones
        r.select_set(True)
        for c in r.children_recursive:
            c.select_set(True)
    bpy.ops.export_scene.gltf(filepath=str(path), export_format="GLB", use_selection=True, export_apply=True,
                              export_yup=True, export_texcoords=False, export_cameras=False, export_lights=False)
    data = path.read_bytes()
    gz = gzip.compress(data, 9, mtime=0)
    path.with_name(path.name + ".gz").write_bytes(gz)  # served to browsers that accept gzip
    print("wrote", path, len(data), "bytes,", len(gz), "gzipped")


OUT.mkdir(parents=True, exist_ok=True)

reset()
pieces = [desk, chair, round_table, sofa, coffee_table, plant, whiteboard, shelf, lamp, cooler]
kit = [f((i % 5) * 3.2, (i // 5) * 3.2) for i, f in enumerate(pieces)]
if PREVIEW:
    preview(PREVIEW / "office-kit.png", (6.4, 1.6, 0.4), 13)
export(OUT / "office-kit.glb", kit)

reset()
team = [chibi(0.0)]
if PREVIEW:
    preview(PREVIEW / "chibi.png", (0, 0, 0.8), 3.2)
export(OUT / "chibi.glb", team)
