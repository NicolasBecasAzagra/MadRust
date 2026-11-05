# Auditoría de diseño — MadRust (sitio anterior al rediseño)

Registro de lo que se encontró antes de tocar nada, y de lo que se decidió
conservar. Las capturas están en `.preview/` (no se versionan).

## 1. Se lee como AI slop

1. **Inter/system-ui como tipografía principal.** `package.json` usa `@fontsource-variable/inter`; el CSS declara `-apple-system, BlinkMacSystemFont, "Inter Variable", system-ui, Roboto, sans-serif`. Es la fuente por defecto del SaaS genérico.
2. **Hero simétrico tipo landing comercial.** Texto grande a la izquierda, panel ilustrativo a la derecha en una cuadrícula de `1.2fr / 0.55fr`. Es el layout de plantilla, no de un kernel.
3. **Etiquetas eyebrow sobre cada título.** "Architecture", "Measurements", "Milestones", "Commands", "Repository" — el patrón etiqueta + título se repite en todas las secciones.
4. **Mismo ADN de tarjeta en todo el sitio.** Capas, benchmarks, caveats, FAQ, comandos, changelog, hitos y layout del repo comparten `bg-elevated`, `border`, `border-radius: 28px` y `box-shadow: 0 4px 20px rgba(0,0,0,.05)`.
5. **Números 01-04 decorativos.** La sección "Four layers" rellena con `padStart(2,'0')` capas que no son una secuencia de pasos.
6. **Sombras blandas genéricas.** `0 24px 80px rgba(0,0,0,.08)` y `0 4px 20px rgba(0,0,0,.05)` son el sello de las plantillas actuales.
7. **Degradados y glassmorphism.** El hero usa `mask-image: linear-gradient(...)` sobre una cuadrícula decorativa; la navegación usa `backdrop-filter: blur(12px) saturate(180%)` con fondo semitransparente.
8. **Píldoras y badges redondos.** Crates, badges de estado y tipos de commit usan `border-radius: 999px` (SaaS kit).
9. **Copy de marketing vacío.** El CTA dice "Boot it. Measure it. Build on it."; los botones repiten "Star on GitHub" / "Read the docs". No hay voz de manual de ingeniería.
10. **Títulos que enuncian lo obvio.** "Four layers, one system", "The numbers, with the losses" — buenos datos, pero la voz es genérica y autopromocional.

## 2. Lo que ya funciona y hay que conservar

- El **terminal del hero** con el log real (`[0.000] MadRust: boot OK`, `MM DIRECTMAP OK`, `INFER OK tokens=42`) es contenido, no decoración.
- Las **tablas de benchmarks** comparan con Linux 7.3.0 e incluyen las pérdidas; ese tono honesto es valioso.
- El **escalonado por fases** del roadmap con `Exit:` y `Verification:` es una estructura de información sólida.
- El **changelog generado desde commits reales** del repositorio evita inventar hitos.
- El **sistema de tokens CSS** (`--bg`, `--text`, `--accent`, `--radius`, `--space-*`) está bien organizado y facilita el rediseño.

## 3. Deuda técnica de diseño

- **Un solo componente "tarjeta" domina todo.** No hay jerarquía entre información principal y secundaria.
- **Ritmo visual plano.** Todas las secciones usan `padding-block: 10rem` y la misma cabecera; la página cae en un compás repetido.
- **Falta de densidad en datos técnicos.** Comandos de build, layout de crates y changelog podrían ser más compactos; ahora respiran como bloques de marketing.
- **Jerarquía tipográfica confusa.** El `h1` del hero fuerza `clamp(2.75rem, 5.5vw, 3.5rem)`, más pequeño que `.text-display` (`5.5rem`), y el cuerpo es `1rem / leading-relaxed` en todo el sitio.
- **Desalineaciones.** La cabecera fija hardcodea `rgba(248,248,250,.82)` que no coincide con `--bg`; en móvil el menú se abre como dos cajas flotantes con posiciones absolutas arbitrarias (`12.5rem`).
- **Oscuro insuficiente.** El tema oscuro invierte colores pero conserva sombras suaves, bordes grises y acento anaranjado sin atmósfera propia.

## 4. Propuesta de dirección

Todas parten del encargo: estética de ingeniería, mono como tipografía de primer nivel, una sola regla de 1 px visible, un solo acento, sin glassmorphism ni degradados.

### A. Consola serie / cuaderno de laboratorio
Feeling: kernel arrancando en un terminal limpio. Tipo: **JetBrains Mono** o **IBM Plex Mono** para todo (display 800 + body 400). Paleta: papel `#f7f5f0` / tinta `#1a1a1a` / acento óxido `#b7410e`; oscuro `#0d0d0d` sobre `#e8e6e1`. Layout: columna ancha, reglas horizontales de 1 px, hero como terminal a pantalla completa. Riesgo: puede parecer frío si el acento y el contraste no calientan.

### B. Panel de instrumentación / osciloscopio
Feeling: leer un rig de medición. Tipo: **Martian Mono** condensada para display y numerales grandes + **Azeret Mono** o IBM Plex Mono para cuerpo. Paleta: grafito `#121212` / rejilla `#2a2a2a` / acento ámbar `#f5a623` (o verde fosforo `#33ff00` en oscuro). Layout: retícula visible, ejes verticales, números grandes como elementos gráficos. Riesgo: el ámbar/verde puede leerse como estética "hacker" de los 90 si se dosifica mal.

### C. Manual de ingeniería técnica (alternativa)
Feeling: documentación impresa de un laboratorio. Tipo: **Newsreader** o **Instrument Serif** solo para titulares grandes + **IBM Plex Mono** para todo lo demás (mono sigue siendo primer nivel en cuerpo, datos y navegación). Paleta: papel crudo `#f4f1ea` / tinta `#111` / acento óxido `#b7410e`; oscuro `#0a0a0a` / `#e0ddd5`. Layout: columnas editoriales, reglas finas, notas al margen con hashes de commit. Riesgo: la serif puede desentonar con la materia de kernel; requiere mucha disciplina.

**Recomendación: A**, con el terminal del hero como momento memorable y la tabla de benchmarks como prueba principal.

## 5. Las 5 decisiones que hay que tomar antes de maquetar

1. **Tipografía display + mono.** (a) Mono única: JetBrains Mono 800/400. (b) Mono display + mono body: Martian Mono 800 + IBM Plex Mono 400. (c) Sans geométrica + mono: Space Grotesk 700 + IBM Plex Mono 400. El encargo empuja a (a) o (b).
2. **Paleta exacta.** Fijar papel (`#f7f5f0`), tinta (`#1a1a1a`), acento (`#b7410e` o `#c45c2f`) y su inverso oscuro (`#0d0d0d` / `#e8e6e1`). Una sola regla: `1px solid var(--border-subtle)`.
3. **Escala tipográfica.** Definir pasos concretos (ej. 12/14/16/20/28/40/64 px) y pesos; abandonar el `clamp` de marketing y usar una escala más dura.
4. **Densidad.** Decidir altura de fila en tablas y listas técnicas (ej. 44 px) vs. secciones explicativas (64 px entre párrafos). La página necesita zonas densas y zonas de aire, no un solo compás.
5. **Tratamiento del hero.** El log de arranque debe mecanografiarse en vivo con cursor parpadeante, respetando `prefers-reduced-motion`; el terminal debe ser el protagonista, no un panel decorativo a la derecha.
