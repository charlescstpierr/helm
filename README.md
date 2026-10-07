# helm

Kanban auto-hébergé en Rust pour développeur solo, destiné à orchestrer des agents de code
(`claude -p`, `codex exec`). Cette version livre le tableau et les commentaires ;
l'orchestration est conçue dans [docs/architecture.md](docs/architecture.md) et viendra ensuite.

## Lancer

Il faut Rust stable (≥ 1.85) et un compilateur C (SQLite est compilé avec le binaire). Ni
Node ni étape de build front.

```sh
cargo run --release
# helm: listening on http://127.0.0.1:7878 (database: helm.db)
```

Le binaire `target/release/helm` est autonome : pages, CSS et JavaScript y sont embarqués, et
la base est créée et migrée au premier démarrage. `Ctrl-C` arrête proprement.

Dans le tableau : flèches pour naviguer, `Alt` + flèches pour déplacer une carte, `Entrée`
pour l'ouvrir, `N` pour en créer une.

Chaque carte a un fil de commentaires, sous son formulaire d'édition (`Ctrl` + `Entrée` envoie).
`@claude`, `@codex` et `@moi` y sont reconnus et enregistrés comme mentions non traitées ; rien
ne les consomme encore. Le nombre de commentaires s'affiche sur la carte quand il n'est pas nul.

## Configurer

Tout est optionnel. Ordre de priorité : variables d'environnement, puis fichier, puis défauts.

| Fichier (`helm.toml`) | Environnement         | Défaut           | Rôle                                           |
| --------------------- | --------------------- | ---------------- | ---------------------------------------------- |
| `bind`                | `HELM_BIND`           | `127.0.0.1:7878` | Adresse d'écoute.                              |
| `db_path`             | `HELM_DB`             | `helm.db`        | Fichier SQLite (mode WAL).                     |

Le fichier lu est `helm.toml` dans le répertoire courant s'il existe, ou celui désigné par
`HELM_CONFIG`. Un `helm.toml` absent laisse les défauts ; un fichier présent mais illisible ou
invalide (clé inconnue comprise), ou un `HELM_CONFIG` introuvable, fait échouer le démarrage.
Modèle : [helm.example.toml](helm.example.toml).

Helm n'a pas d'authentification : gardez l'écoute sur loopback, ou placez un proxy ou un VPN de
confiance devant.

## Architecture

Un seul crate binaire : Axum sur un runtime Tokio mono-thread sert des pages rendues côté
serveur (gabarits Askama compilés) ; les données vivent dans un fichier SQLite en WAL, via une
unique connexion rusqlite et des migrations versionnées embarquées. Chaque modification publie
un événement SSE, et les navigateurs connectés rechargent le fragment du tableau. Le front est
du JavaScript sans dépendance, en amélioration progressive (les formulaires fonctionnent sans
lui), embarqué avec `rust-embed` et servi sous une CSP stricte, sans CDN. Tout le style passe
par les variables CSS de [`assets/tokens.css`](assets/tokens.css) — c'est le seul fichier à
modifier pour appliquer une charte visuelle, thème clair et sombre compris. Détails, modèle de
données, cycle de vie des cartes et conception de l'orchestrateur :
[docs/architecture.md](docs/architecture.md).

## Développer

```sh
cargo test                                   # cartes, commentaires et mentions, migrations, routes
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

En build de développement (`cargo run`), `assets/` est relu depuis le disque à chaque requête :
le CSS et le JavaScript se modifient sans recompiler. Les gabarits de `templates/`, eux, sont
compilés.
