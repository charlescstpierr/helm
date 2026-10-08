# helm

Kanban auto-hébergé en Rust pour développeur solo, destiné à orchestrer des agents de code
(`claude -p`, `codex exec`). Cette version livre le tableau, les commentaires et une première
tranche de l'orchestration (Claude, une branche poussée par carte) ; la suite est conçue dans
[docs/architecture.md](docs/architecture.md).

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
| `project.repo`        |                       | aucun            | Dépôt git des cartes (chemin absolu ou `~/…`). Sans lui, pas d'agents. |
| `project.worktree_root` |                     | `~/.local/share/helm/worktrees` | Un worktree par carte : `<racine>/<CLÉ>-<n>` (chemin absolu ou `~/…`). |
| `agents.max_concurrent` |                     | `2`              | Exécutions simultanées au plus.                |
| `agents.run_timeout_minutes` |                | `60`             | Durée maximale d'un agent ; au-delà il est arrêté et l'exécution échoue. |
| `agents.claude.command` |                     | `claude`         | Exécutable de Claude Code.                     |
| `agents.claude.permission_mode` |             | `bypassPermissions` | `--permission-mode` des exécutions.         |
| `agents.claude.model` |                       | celui du CLI     | Modèle des cartes qui n'en nomment pas.        |

Le fichier lu est `helm.toml` dans le répertoire courant s'il existe, ou celui désigné par
`HELM_CONFIG`. Un `helm.toml` absent laisse les défauts ; un fichier présent mais illisible ou
invalide (clé inconnue comprise), ou un `HELM_CONFIG` introuvable, fait échouer le démarrage.
Les chemins de `project.repo` et `project.worktree_root` doivent être absolus ou commencer par
`~/`. Un worktree existant n'est réutilisé que s'il appartient au dépôt configuré
et se trouve sur la branche de la carte ; un dossier incompatible est refusé sans être écrasé.
Modèle : [helm.example.toml](helm.example.toml).

Helm n'a pas d'authentification : gardez l'écoute sur loopback, ou placez un proxy ou un VPN de
confiance devant.

## Lancer des agents

Renseignez `project.repo` dans `helm.toml` (un dépôt git avec un remote `origin`) et gardez
`bind` sur loopback : sans dépôt, l'assignation d'un agent est indisponible ; hors loopback, Helm
refuse de lancer quoi que ce soit, parce qu'une exécution est de l'exécution de code avec vos
droits et que rien n'authentifie les requêtes.

Dans le formulaire d'une carte, choisissez l'agent (Claude) et, si besoin, un modèle (vide : celui
de `agents.claude.model`, sinon celui du CLI). Quand la carte **entre** dans une colonne « À
faire », une exécution est mise en file : Helm crée le worktree `<racine>/<CLÉ>-<n>` sur la
branche `helm/<CLÉ>-<n>`, lance `claude -p` dedans (mode `bypassPermissions` par défaut), garde
chaque événement du flux (affiché en direct dans « Activité de l'agent » : statut, branche, session, coût, jetons, journal, sortie d'erreur, consigne), puis pousse vers `origin` les commits de la branche qu'`origin` n'a pas encore (ceux de cette exécution ou d'une précédente dont le push avait échoué). Succès : la carte
passe en « En revue » si elle est encore « En cours » (déplacée à la main entre-temps, elle y reste). Échec (agent, rien de nouveau à pousser, `origin` injoignable ou divergent, push refusé) : elle reste « En cours » et
l'erreur est affichée sur la carte et ajoutée à son fil. Aucune PR n'est ouverte et les worktrees ne sont pas nettoyés.
Une ligne de sortie de plus de 1 Mio est tronquée (son début est gardé, le journal le signale) sans
interrompre l'exécution. Un agent encore actif après `agents.run_timeout_minutes` est arrêté comme
par une annulation, et l'exécution échoue avec la limite pour motif.
Une exécution en cours peut être annulée depuis la carte. Un arrêt normal de Helm (SIGINT, SIGTERM)
arrête les agents de la même façon (SIGTERM au groupe de processus, puis SIGKILL après 3 s) et marque
l'exécution « interrompue ». Si Helm est tué sans préavis, l'exécution est marquée « interrompue »
au redémarrage ; elle n'est jamais relancée seule, et si son agent tourne encore, une nouvelle
exécution sur la même carte est refusée tant que ce groupe de processus existe (seule la dernière
exécution ayant lancé un agent compte). Le message dit de vérifier ce qu'est ce processus avant de
l'arrêter (`kill -- -<pid>`), car Helm ne peut pas le distinguer d'un processus qui aurait reçu le
même numéro.

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
cargo test                                   # cartes, commentaires, migrations, routes, orchestrateur
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Les tests de l'orchestrateur n'appellent ni `claude` ni le réseau : ils lancent
`tests/fixtures/fake-claude.sh`, qui rejoue des flux enregistrés (le scénario est la ligne
`SCENARIO: …` de la consigne), avec un vrai `git` et un dépôt `origin` nu temporaire.

En build de développement (`cargo run`), `assets/` est relu depuis le disque à chaque requête :
le CSS et le JavaScript se modifient sans recompiler. Les gabarits de `templates/`, eux, sont
compilés.
