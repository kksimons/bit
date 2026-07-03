import * as THREE from "three";
import { type BitState, buildGeometry, COLORS } from "./shapes";

const ROTATION_SPEED: Record<BitState, number> = {
  neutral: 0.008,
  listening: 0.014,
  thinking: 0.03,
  yes: 0.025,
  no: 0.016,
};

const TRANSITION_S = 0.3;
const MIN_SCALE = 0.22;

// Personality bounce: on yes/no the Bit bobs UP (yes) or DOWN (no). Bigger
// answers (times=3) bounce higher and quicker. Layered on top of the resting
// hover-bob so it composes with the form-change and spin.
const BOUNCE_DURATION_S = 0.6;
const BOUNCE_AMPLITUDE = 0.28; // world-space y units at times=1

interface Bounce {
  active: boolean;
  t: number;
  direction: 1 | -1; // +1 = yes (up), -1 = no (down)
  amplitude: number;
  speed: number;
}

interface Transition {
  active: boolean;
  t: number;
  to: BitState;
  swapped: boolean;
}

/**
 * Renders the Bit: a flat-shaded emissive polyhedron on a transparent canvas.
 * Form changes contract to a flicker, swap geometry at the smallest point, then
 * expand back out — the same symmetric motion in both directions.
 */
export class Bit {
  private renderer: THREE.WebGLRenderer;
  private scene = new THREE.Scene();
  private camera: THREE.PerspectiveCamera;
  private material: THREE.MeshPhongMaterial;
  private mesh: THREE.Mesh;
  private state: BitState = "neutral";
  private clock = new THREE.Clock();
  private revertTimer: number | null = null;
  private transition: Transition = {
    active: false,
    t: 0,
    to: "neutral",
    swapped: true,
  };
  private bounce: Bounce = {
    active: false,
    t: 0,
    direction: 1,
    amplitude: 0,
    speed: 1,
  };
  // Thinking sway: while processing, the Bit drifts, alternating the axis
  // (x side-to-side ↔ y up-and-down) on each backend "step" nudge.
  private thinkAxis: 0 | 1 = 0;
  private thinkStepAt = 0;
  private sounds: Record<"yes" | "no", HTMLAudioElement> = {
    yes: new Audio("/sounds/bit-yes.mp3"),
    no: new Audio("/sounds/bit-no.mp3"),
  };

  constructor(canvas: HTMLCanvasElement) {
    this.renderer = new THREE.WebGLRenderer({
      canvas,
      alpha: true,
      antialias: true,
    });
    this.renderer.setClearColor(0x000000, 0);
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping;
    this.renderer.toneMappingExposure = 1.1;

    this.camera = new THREE.PerspectiveCamera(50, 1, 0.1, 100);
    this.camera.position.set(0, 0, 4.4);

    const key = new THREE.DirectionalLight(0xffffff, 2.6);
    key.position.set(3, 5, 4);
    this.scene.add(key);
    const rim = new THREE.DirectionalLight(0x33ccff, 1.4);
    rim.position.set(-4, -2, -3);
    this.scene.add(rim);
    this.scene.add(new THREE.AmbientLight(0xffffff, 0.7));

    this.material = new THREE.MeshPhongMaterial({
      flatShading: true,
      shininess: 90,
      specular: 0xffffff,
      side: THREE.DoubleSide,
      emissiveIntensity: 0.85,
    });
    this.mesh = new THREE.Mesh(buildGeometry("neutral"), this.material);
    this.scene.add(this.mesh);

    this.applyPalette();
    this.resize();
    window.addEventListener("resize", () => this.resize());
    this.animate();
  }

  /** Begin a symmetric transition to a new state. yes/no auto-revert. */
  setState(state: BitState, revertMs = 1500) {
    if (this.revertTimer !== null) {
      window.clearTimeout(this.revertTimer);
      this.revertTimer = null;
    }
    this.transition = { active: true, t: 0, to: state, swapped: false };
    if (state === "yes" || state === "no") {
      this.startBounce(state);
    }
    if (revertMs > 0 && (state === "yes" || state === "no")) {
      this.revertTimer = window.setTimeout(() => this.setState("neutral", 0), revertMs);
    }
  }

  private enthusiasm = 1; // 1..3, set by react()
  private startBounce(kind: "yes" | "no") {
    const e = this.enthusiasm;
    this.bounce = {
      active: true,
      t: 0,
      direction: kind === "yes" ? 1 : -1,
      amplitude: BOUNCE_AMPLITUDE * (0.7 + (e - 1) * 0.35),
      speed: 1 + (e - 1) * 0.25,
    };
  }

  /** Say yes/no `times` (1–3) in quick succession — Bit's bit of personality. */
  react(kind: "yes" | "no", times: number) {
    const n = Math.max(1, Math.min(3, Math.round(times)));
    this.enthusiasm = n;
    const interval = 430;
    for (let i = 0; i < n; i++) {
      const last = i === n - 1;
      window.setTimeout(() => this.setState(kind, last ? 1400 : 0), i * interval);
    }
  }

  /** Flip the thinking-sway axis (x ↔ y) and re-phase it, so motion visibly
   * changes direction as work progresses. */
  thinkingStep() {
    this.thinkAxis = this.thinkAxis === 0 ? 1 : 0;
    this.thinkStepAt = this.clock.elapsedTime;
  }

  private swapTo(state: BitState) {
    this.state = state;
    this.mesh.geometry.dispose();
    this.mesh.geometry = buildGeometry(state);
    this.applyPalette();
    if (state === "yes" || state === "no") this.playVoice(state);
  }

  private playVoice(state: "yes" | "no") {
    const a = this.sounds[state];
    a.currentTime = 0;
    void a.play().catch(() => {
      /* autoplay may be blocked before first user gesture */
    });
  }

  private applyPalette() {
    const p = COLORS[this.state];
    this.material.color.setHex(p.color);
    this.material.emissive.setHex(p.emissive);
  }

  /// Advance the bounce by `dt`, returning its current y offset. A single
  /// symmetric sin hop — peaks at the midpoint, returns to 0 at both ends.
  private bounceOffset(dt: number): number {
    if (!this.bounce.active) return 0;
    this.bounce.t += dt * this.bounce.speed;
    const p = this.bounce.t / BOUNCE_DURATION_S;
    if (p >= 1) {
      this.bounce.active = false;
      return 0;
    }
    const hop = Math.sin(p * Math.PI);
    return this.bounce.direction * this.bounce.amplitude * hop;
  }

  private resize() {
    const w = window.innerWidth;
    const h = window.innerHeight;
    this.renderer.setSize(w, h, false);
    this.camera.aspect = w / h;
    this.camera.updateProjectionMatrix();
  }

  private animate = () => {
    requestAnimationFrame(this.animate);
    const dt = this.clock.getDelta();
    const t = this.clock.elapsedTime;

    const speed = ROTATION_SPEED[this.state];
    this.mesh.rotation.y += speed;
    this.mesh.rotation.x += speed * 0.4;
    // no: a gentle side-to-side head-shake (the universal "no" gesture).
    if (this.state === "no") {
      this.mesh.rotation.z = Math.sin(t * 14) * 0.09;
    } else {
      this.mesh.rotation.z *= 0.85; // ease z back to 0 after a no
    }
    // Resting hover-bob is the base; the bounce and thinking sway add on top.
    let swayY = 0;
    if (this.state === "thinking") {
      const since = t - this.thinkStepAt;
      const env = Math.min(1, since * 3.2);
      const s = Math.sin(since * 2.4) * 0.14 * env;
      if (this.thinkAxis === 0) this.mesh.position.x = s;
      else swayY = s;
    } else {
      this.mesh.position.x *= 0.8;
    }
    this.mesh.position.y = Math.sin(t * 1.5) * 0.12 + this.bounceOffset(dt) + swayY;

    let scale: number;
    if (this.transition.active) {
      this.transition.t += dt;
      const p = Math.min(this.transition.t / TRANSITION_S, 1);
      if (!this.transition.swapped && p >= 0.5) {
        this.transition.swapped = true;
        this.swapTo(this.transition.to);
      }
      // symmetric dip: 1 -> MIN_SCALE at midpoint -> 1
      scale = 1 - (1 - MIN_SCALE) * Math.sin(p * Math.PI);
      this.material.emissiveIntensity = 0.85;
      if (p >= 1) this.transition.active = false;
    } else if (this.state === "listening") {
      // strong "breathing" pulse + glow throb so it's obviously listening.
      const pulse = (Math.sin(t * 7) + 1) / 2;
      scale = 1.06 + pulse * 0.14;
      this.material.emissiveIntensity = 0.6 + pulse * 1.1;
    } else if (this.state === "thinking") {
      const pulse = (Math.sin(t * 12) + 1) / 2;
      scale = 1;
      this.material.emissiveIntensity = 0.7 + pulse * 0.7;
    } else {
      scale = this.state === "neutral" ? 1 + Math.sin(t * 1.2) * 0.02 : 1;
      // yes: brighter when said more emphatically; no: dimmer/deflated.
      if (this.state === "yes") {
        this.material.emissiveIntensity = 0.85 + (this.enthusiasm - 1) * 0.25;
      } else if (this.state === "no") {
        this.material.emissiveIntensity = 0.7;
      } else {
        this.material.emissiveIntensity = 0.85;
      }
    }
    this.mesh.scale.setScalar(scale);

    this.renderer.render(this.scene, this.camera);
  };
}
