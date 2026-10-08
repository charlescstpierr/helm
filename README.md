# helm

Kanban auto-hébergé en Rust pour développeur solo. Helm lance Claude dans un worktree par
carte, exécute vos vérifications, pousse la branche et peut ouvrir une PR GitHub prête à
relire. La fusion reste une action explicite. Le tableau et les commentaires fonctionnent
aussi sans agent ni GitHub. Voir [docs/architecture.md](docs/architecture.md).

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
| `checks.commands` |                           | `[]`             | Commandes shell exécutées dans l'ordre, dans le worktree, avant le push. |
| `checks.timeout_minutes` |                    | `10`             | Durée maximale de chaque commande de vérification. |
| `github.repository` |                         | aucun            | Dépôt sur github.com, au format `owner/repo`. Active les PR GitHub. |
| `github.command` |                            | `gh`             | Exécutable du CLI GitHub, trouvé dans `PATH` ou désigné par son chemin. |

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
l'erreur est affichée sur la carte et ajoutée à son fil. Les worktrees ne sont pas nettoyés
automatiquement. Les vérifications et les PR se configurent séparément, comme décrit ci-dessous.
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

## Vérifier le travail avant le push

Ajoutez les commandes de votre projet dans `helm.toml`. Par exemple, pour ce dépôt Rust :

```toml
[checks]
commands = [
    "cargo fmt --check",
    "cargo clippy --all-targets --locked -- -D warnings",
    "cargo test --locked",
]
timeout_minutes = 10
```

Après le succès de l'agent, Helm exécute chaque chaîne avec `sh -c` dans le worktree de la
carte. Les commandes partagent les droits et l'environnement du processus Helm. Au premier
échec ou dépassement de délai, Helm arrête les vérifications et ne pousse rien. Une annulation
arrête aussi le groupe de processus du contrôle en cours.

Les résultats portent sur un commit précis. Quand des commandes ou GitHub sont configurés,
Helm exige un worktree propre et la branche attendue avant et après les contrôles, puis juste
avant le push. Un changement du commit ou des fichiers bloque la publication. Helm pousse le
commit vérifié, sans forcer.

La carte affiche le SHA, chaque commande, son état, son code de retour et ses sorties standard
et d'erreur. Chaque sortie est limitée à 1 Mio, avec une mention si elle est tronquée.
Sans commande configurée, les vérifications sont « Non exécutées », jamais « Réussies ».
Les anciens fichiers de configuration restent valides.

## Publier et fusionner une PR GitHub

Installez [GitHub CLI](https://cli.github.com/) sur la machine qui lance Helm.
Authentifiez-le avec un compte autorisé à pousser et à gérer les PR du dépôt :

```sh
gh auth login --hostname github.com
gh auth status --hostname github.com
```

Configurez le dépôt GitHub correspondant au remote `origin` de `project.repo` :

```toml
[github]
repository = "owner/repo"
# command = "/chemin/vers/gh" # sinon, gh dans PATH
```

Sans section `[github]`, cette intégration reste désactivée. Elle exige aussi `project.repo`
et une écoute sur loopback. Cette version utilise github.com.

Après le push, Helm crée ou retrouve la PR de la branche vers la branche par défaut du dépôt
et la rend prête à relire. Lors de la création, son corps indique le commit vérifié et les
résultats locaux.
Le lien apparaît dans l'activité et le fil de la carte. Si GitHub est indisponible ou si la
publication de la PR est annulée après le push, l'exécution reste réussie. Le bouton
« Créer la PR / Réessayer » reprend la publication sans relancer l'agent.

L'activité affiche l'état de la PR, ses contrôles CI et la dernière actualisation.
« Aucun contrôle » signifie qu'aucun contrôle CI n'a été déclaré, pas qu'un contrôle a réussi.
Helm actualise les PR des cartes non terminées en arrière-plan, avec une période de 30 secondes.
« Actualiser la PR » permet aussi une actualisation manuelle.

« Fusionner la PR » demande une fusion squash. Helm relit GitHub, vérifie le commit, la branche
et la base attendus, puis transmet le SHA attendu à `gh`. Une PR en brouillon, une CI en cours,
échouée ou inconnue, une revue bloquante ou une protection GitHub empêche la fusion.
Helm ne force pas et n'utilise pas de privilège administrateur. L'absence de vérifications
locales ou de contrôles CI reste visible et n'empêche pas à elle seule une fusion explicite.

Helm passe automatiquement la carte à « Terminé » seulement après confirmation de la fusion
de la PR correspondant à la dernière exécution de la carte. Cette exécution doit être réussie,
et son commit vérifié ainsi que les branches doivent correspondre à la PR publiée. Une exécution
active ou une PR périmée bloque cette mise à jour. Une fusion effectuée directement sur GitHub
est reconnue à l'actualisation. Les déplacements manuels de carte restent possibles.
Les boutons de publication, d'actualisation et de fusion sont des formulaires POST utilisables
avec ou sans JavaScript.

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
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Les tests n'appellent ni Claude ni GitHub réels. `tests/fixtures/fake-claude.sh` rejoue des
flux enregistrés, selon la ligne `SCENARIO: …` de la consigne. `tests/fixtures/fake-gh.sh`
simule les réponses et les mutations de PR. Les parcours utilisent un vrai `git`, des dépôts
temporaires et un `origin` nu local.

En build de développement (`cargo run`), `assets/` est relu depuis le disque à chaque requête :
le CSS et le JavaScript se modifient sans recompiler. Les gabarits de `templates/`, eux, sont
compilés.
