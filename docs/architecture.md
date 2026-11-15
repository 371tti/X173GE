# Architecture contracts

## Ownership

The game thread owns input, Shipyard world, fixed update and render extraction. It sends owned snapshots over a bounded channel. A dedicated renderer thread owns the backend and every Vulkan object. The renderer never reads ECS storage. The platform owns the native window for longer than the renderer's surface.

## Engine invariants

The rebuild1 checkout supplies the unchanged comparison scene and behavioral
observations only. Its asset importer and renderer are not implementation
references; asset-format behavior follows the Khronos glTF specification.

- Surface generations are monotonic per `SurfaceId`; an older configure request cannot replace a newer one. A renderer `OutOfDate` result is a distinct invalidation event, so the platform can recreate at the same extent with a newer generation.
- An invalidated generation is not sent to the backend again. In-flight frames from it may be dropped while the newer generation is configured. A backend operation error ends the renderer service because Vulkan acquire/submit failures can leave synchronization objects unsafe to reuse.
- A bounded channel stays bounded end to end. A consumer cannot drain commands into an unbounded pending-import list; concurrent and pending imports have explicit limits and reject overload visibly.
- Fixed updates use a 60 Hz default, at most four catch-up ticks per rendered frame, and interpolation for render extraction. Gameplay state is not advanced by render frequency.
- `FramePacing` independently caps frame extraction/submission and selects FIFO or immediate-preferred presentation. A live VSync change advances the surface generation and recreates the swapchain through the bounded renderer command queue; the default cap is 60 FPS with VSync enabled.
- Input transitions remain pending until a fixed tick consumes them. The runtime moves the input state into a Shipyard `Unique` for the tick and back afterward, so ECS workloads can read it without a per-tick hash-set clone. Minimized windows stop frame submission and recreate the surface at a new generation on restore.
- The game thread handles renderer events inline, capped at 64 per wait cycle; it does not stage a burst in an unbounded temporary collection.
- Shutdown closes upload admission atomically, waits only for already-entered nonblocking sends, drains bounded events if needed, and joins the renderer before the window can be dropped. Vulkan image acquisition has a finite timeout so it returns to the command loop if the surface stops producing images.
- Vulkan swapchain/resource lifetimes are kept in the Vulkan crate. Resize waits and resource recreation are measured before replacing safe device-idle synchronization with finer-grained lifetime tracking.
- Passing `oldSwapchain` to `vkCreateSwapchainKHR` retires it even if replacement creation fails, so a failed backend configuration is terminal and the backend is destroyed instead of rolled back. [Khronos Vulkan Specification](https://registry.khronos.org/vulkan/specs/latest-ratified/pdf/vkspec.pdf)
- The Vulkan crate's fixed, single-queue frame graph declares pass order and logical image reads/writes. It rejects read-before-write and command recording that diverges from that order. Vulkan render passes perform image layout transitions and synchronization; common layouts and external dependency scopes are centralized with the graph. The MBOIT moment attachments use additive blending and explicit attachment-to-sampler dependencies in the shadow render pass. Image creation and lifetime stay with Vulkan resource owners, and upload barriers remain outside the frame graph. Resource aliasing or pass fusion is added only after measurements show a bottleneck.
- Asset load errors are explicit. The renderer never inserts a placeholder mesh or material.

## Current renderer scope

`x173ge-renderer` defines the backend contract and Null backend. The Vulkan crate owns scene targets, GPU uploads, and the draw passes. The current importer-to-renderer path supports the documented glTF 2.0 static-scene subset, PBR materials, directional, point, and spot lights from `KHR_lights_punctual`, CSM with PCSS and optional MBOIT translucent shadows, GTAO, Bloom, SMAA, feature-gated SSR, and homogeneous single-scattering God Rays. Point and spot lights use inverse-square falloff and do not cast shadows; God Rays continue to use directional lights only. God Rays reuse scene depth and CSM, use spatial interleaving with depth-edge fallback, and do not correctly fog transparent surfaces. `KHR_materials_transmission` values import, but visible transmission shading is not implemented. The supported asset subset and its spec checks are listed in [`research/gltf-core-import.md`](research/gltf-core-import.md).

The protocol contains only owned engine data and native surface descriptors. It contains no Vulkan handles, ECS entities, pointers into user memory, or references. The ECS crate reuses Shipyard storage and scheduling without a second scheduler.
