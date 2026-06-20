import * as THREE from "three";

export type BitState = "neutral" | "listening" | "thinking" | "yes" | "no";

export interface BitPalette {
  color: number;
  emissive: number;
}

// Bit's per-state colors.
export const COLORS: Record<BitState, BitPalette> = {
  neutral: { color: 0x00ddff, emissive: 0x0a7d96 },
  listening: { color: 0x46e8ff, emissive: 0x179fbe },
  thinking: { color: 0x8ffaff, emissive: 0x2bc6d6 },
  yes: { color: 0xffdd00, emissive: 0x9c7a00 },
  no: { color: 0xff3300, emissive: 0x9c1300 },
};

// Deterministic pseudo-random in [0,1) seeded by a face normal, so the
// "no" stellation's irregular spikes are stable frame-to-frame.
function pseudoRandom(n: THREE.Vector3): number {
  const s = Math.sin(n.x * 127.1 + n.y * 311.7 + n.z * 74.7) * 43758.5453;
  return s - Math.floor(s);
}

const keyOf = (v: THREE.Vector3) => `${v.x.toFixed(3)},${v.y.toFixed(3)},${v.z.toFixed(3)}`;

/**
 * Stellate a convex polyhedron: replace each (coplanar) face with a pyramid
 * whose apex is pushed out along the face normal. Works on any Three.js
 * polyhedron geometry by grouping its triangles into faces by shared normal,
 * then fanning the face's boundary edges up to a single apex.
 *
 *  - icosahedron base -> 20 triangular spikes (neutral Bit)
 *  - dodecahedron base -> 12 pentagonal spikes (no Bit)
 *
 * @param spikeHeight how far each apex is pushed beyond the face centroid
 * @param randomize   0 = uniform spikes; >0 jitters per-face height by ±randomize
 */
export function stellate(
  base: THREE.BufferGeometry,
  spikeHeight: number,
  randomize = 0,
): THREE.BufferGeometry {
  const pos = base.getAttribute("position") as THREE.BufferAttribute;
  const triCount = pos.count / 3;

  interface Face {
    normal: THREE.Vector3;
    tris: number[];
  }
  const faces = new Map<string, Face>();

  const a = new THREE.Vector3();
  const b = new THREE.Vector3();
  const c = new THREE.Vector3();
  const ab = new THREE.Vector3();
  const ac = new THREE.Vector3();
  const n = new THREE.Vector3();

  for (let t = 0; t < triCount; t++) {
    a.fromBufferAttribute(pos, t * 3 + 0);
    b.fromBufferAttribute(pos, t * 3 + 1);
    c.fromBufferAttribute(pos, t * 3 + 2);
    n.crossVectors(ab.subVectors(b, a), ac.subVectors(c, a)).normalize();
    const key = `${n.x.toFixed(2)},${n.y.toFixed(2)},${n.z.toFixed(2)}`;
    let face = faces.get(key);
    if (!face) {
      face = { normal: n.clone(), tris: [] };
      faces.set(key, face);
    }
    face.tris.push(t);
  }

  const out: number[] = [];

  for (const face of faces.values()) {
    const centroid = new THREE.Vector3();
    let count = 0;
    // boundary edges appear in exactly one triangle of the face group
    const edges = new Map<string, { count: number; a: THREE.Vector3; b: THREE.Vector3 }>();

    for (const t of face.tris) {
      const tv = [
        new THREE.Vector3().fromBufferAttribute(pos, t * 3 + 0),
        new THREE.Vector3().fromBufferAttribute(pos, t * 3 + 1),
        new THREE.Vector3().fromBufferAttribute(pos, t * 3 + 2),
      ];
      for (const p of tv) {
        centroid.add(p);
        count++;
      }
      for (let e = 0; e < 3; e++) {
        const pa = tv[e];
        const pb = tv[(e + 1) % 3];
        const ka = keyOf(pa);
        const kb = keyOf(pb);
        const uk = ka < kb ? `${ka}|${kb}` : `${kb}|${ka}`;
        const existing = edges.get(uk);
        if (existing) existing.count++;
        else edges.set(uk, { count: 1, a: pa, b: pb });
      }
    }
    centroid.multiplyScalar(1 / count);

    let h = spikeHeight;
    if (randomize > 0) {
      const r = pseudoRandom(face.normal);
      h = spikeHeight * (1 - randomize + 2 * randomize * r);
    }
    const apex = centroid.clone().addScaledVector(face.normal, h);

    for (const edge of edges.values()) {
      if (edge.count !== 1) continue; // interior edge, skip
      out.push(edge.a.x, edge.a.y, edge.a.z, edge.b.x, edge.b.y, edge.b.z, apex.x, apex.y, apex.z);
    }
  }

  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.Float32BufferAttribute(out, 3));
  geo.computeVertexNormals();
  return geo;
}

/** Build the exact polyhedron for a given Bit state. */
export function buildGeometry(state: BitState): THREE.BufferGeometry {
  switch (state) {
    case "yes":
      // plain octahedron
      return new THREE.OctahedronGeometry(1.25, 0);
    case "no":
      // stellated dodecahedron: 12 long, irregular, aggressive spikes
      // (kept compact so the spikes stay within the canvas)
      return stellate(new THREE.DodecahedronGeometry(0.8, 0), 0.65, 0.35);
    default:
      // neutral / listening / thinking: gently stellated icosahedron
      return stellate(new THREE.IcosahedronGeometry(0.95, 0), 0.5, 0);
  }
}
