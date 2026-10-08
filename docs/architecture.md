# Architecture de Helm

Helm est un tableau kanban auto-hébergé pour un développeur solo, dans l'esprit de Symphony
(un tableau qui orchestre des agents de code) croisé avec Linear et ClickUp. Les agents sont
des CLI (`claude -p`, `codex exec`) ; le tableau est à la fois l'outil de suivi et le bus par
lequel ils se coordonnent.

Ce document décrit le kanban, ses commentaires, l'orchestration de Claude, les vérifications
locales et les PR GitHub. Les éléments de conception non livrés sont indiqués comme **prévus**.

## 1. Principes

- **Un seul binaire, une seule base.** Un crate Rust, un fichier SQLite, les assets web
  embarqués. Pas de Node, pas d'étape de build front, pas de service annexe.
- **Léger d'abord.** Runtime Tokio à un seul thread, une seule connexion SQLite,
  aucun cache applicatif. La RAM au repos est un objectif mesuré, pas une intention.
- **Local par défaut.** Écoute sur `127.0.0.1`, pas d'authentification : la frontière de
  sécurité est la machine.
- **Le serveur rend le HTML.** Le navigateur reçoit des pages et des fragments ; le JavaScript
  n'est qu'une amélioration progressive (sans lui, les formulaires fonctionnent encore).
- **Le style est une donnée.** Toute valeur visuelle vit dans `assets/tokens.css` ; la charte
  graphique se branche là, sans toucher aux composants.
- **Le tableau est la source de vérité.** Tout ce qu'un agent fait d'observable est écrit sur
  une carte. Rien d'important ne vit seulement dans un terminal.

## 2. Vue d'ensemble (livré)

```
navigateur ──HTTP──▶ axum (routes.rs) ──▶ store.rs ──▶ SQLite (WAL)
     ▲                    │
     └──── SSE /events ◀──┘  (diffusion « le tableau a changé »)
```

| Module          | Rôle                                                                        |
| --------------- | --------------------------------------------------------------------------- |
| `main.rs`       | Arguments, construction du runtime, écoute, arrêt propre.                   |
| `config.rs`     | Valeurs par défaut → fichier TOML → variables d'environnement.              |
| `db.rs`         | Connexion SQLite, pragmas, migrations embarquées, pont vers le pool bloquant. |
| `store.rs`      | Modèle du tableau, opérations sur les cartes et les commentaires (synchrone, testé seul). |
| `mentions.rs`   | Reconnaissance des `@mentions` : cibles connues et scanner pur.             |
| `routes.rs`     | Pages, formulaires, flux SSE, garde-fous de requête.                        |
| `runs.rs`       | Exécutions d'agent : machine à états, journal d'événements, opérations de stockage. |
| `resume.rs`     | Conditions de reprise, résolution de la session et mise en file atomique avec le commentaire. |
| `agent.rs`      | Vocabulaire commun : agent, modèle, mode de permission.                     |
| `adapter.rs`    | `AgentAdapter` : ligne de commande d'un CLI et lecture de son flux (`ClaudeAdapter`). |
| `supervisor.rs` | `Orchestrator` (mise en file, reprise, annulation) et `Supervisor` (processus, worktree, vérifications, push, publication de PR). |
| `git.rs`        | Worktrees, comptage de commits et push, par appel au binaire `git`.         |
| `checks.rs`     | Exécution des commandes locales et stockage de leurs résultats par commit et exécution. |
| `process.rs`    | Commandes externes avec délai maximal, sorties bornées et arrêt du groupe de processus. |
| `github.rs`     | Client du CLI `gh` : création, actualisation et fusion de PR sur github.com. |
| `delivery.rs`   | Publication du commit vérifié, instantanés GitHub, réservation par carte et passage automatique à Terminé. |
| `prompt.rs`     | Consigne envoyée à l'agent, construite depuis la carte et son fil.          |
| `changes.rs`    | Notification « le tableau a changé », partagée par les routes et le superviseur. |
| `assets.rs`     | Fichiers statiques embarqués (`rust-embed`), revalidés par ETag.            |
| `templates/`    | Gabarits Askama compilés dans le binaire.                                   |
| `assets/`       | `tokens.css` (variables), `app.css` (composants), `app.js`, `theme.js`.     |
| `migrations/`   | SQL versionné, inclus à la compilation.                                     |

### Choix techniques

- **Axum sur Tokio**, fonctionnalités réduites au nécessaire (`http1`, `form`). Runtime
  `current_thread`.
- **rusqlite (SQLite compilé avec le binaire)** plutôt que sqlx : pas de pool ni de runtime de
  requêtes asynchrone à payer. Une seule connexion derrière un mutex, utilisée depuis le pool
  bloquant de Tokio (plafonné à 2 threads). Pour un utilisateur, sérialiser les écritures est
  une simplification, pas une limite.
- **WAL** : lecteurs externes (CLI `sqlite3`, sauvegarde, futur outillage d'agent) non bloqués
  par les écritures ; `synchronous = NORMAL`, `foreign_keys = ON`, `busy_timeout` 5 s. Le WAL
  est replié dans le fichier principal à l'arrêt.
- **Migrations** : liste ordonnée dans `db.rs`, table `schema_migrations`, une transaction par
  migration. Une migration publiée ne se modifie jamais. Une base plus récente que le binaire
  est refusée au démarrage plutôt que corrompue.
- **Pas de framework front.** Environ 400 lignes de JavaScript sans dépendance : glisser-déposer
  HTML5, raccourcis clavier, dialogue d'édition, `EventSource`. Rien n'est chargé depuis un
  CDN ; la CSP est `default-src 'self'`.

### Empreinte mesurée

Build `--release` (LTO, symboles retirés) de la v0.1, sur macOS 15.7 x86-64 (Intel Core i9) —
à remesurer sur Apple Silicon :

| Mesure                                                        | Valeur            |
| ------------------------------------------------------------- | ----------------- |
| Taille du binaire, assets embarqués                           | 3,2 Mo            |
| RAM au repos juste après le démarrage (RSS)                   | 2,5 Mo            |
| RAM au repos après 210 requêtes, un client SSE connecté (RSS) | 4,2 Mo            |
| Empreinte physique (`footprint`) dans ces deux états          | 0,8 Mo puis 2,1 Mo |
| Threads au repos                                              | 1                 |

Méthode : `ps -o rss=` et `footprint <pid>` sur le processus, 12 s après la dernière requête.

### Mises à jour en direct

Chaque écriture réussie incrémente une révision et la publie sur un canal `broadcast`.
`GET /events` la relaie en SSE (`event: board`). Le client ne reçoit pas de diff : il recharge
le fragment `GET /board` et remplace les listes de cartes. Un onglet en retard ou reconnecté
converge donc toujours, au prix d'un rendu complet du tableau — négligeable à cette échelle.

Un commentaire publie le même événement : le compteur de la carte se met à jour avec le
tableau, et le dialogue ouvert recharge seulement le fil (`GET /cards/{id}/comments`), jamais le
formulaire, pour ne pas écraser un brouillon en cours de saisie.
Les changements de vérification ou de PR publient aussi ce signal. Le dialogue recharge alors
le panneau d'activité depuis les résultats et l'instantané conservés en base. Lire une page ou
un fragment ne lance aucune commande GitHub et ne modifie pas la base.

### Sécurité du mode local

Sans authentification, tout ce qui atteint le port peut modifier le tableau. Deux garde-fous
empêchent une page web ouverte dans le même navigateur de le piloter :

- les écritures dont l'en-tête `Origin` ne correspond pas à `Host`, ou dont `Sec-Fetch-Site`
  n'est ni `same-origin` ni `none`, sont refusées (anti-CSRF) ;
- quand l'écoute est sur loopback, seuls les noms d'hôte loopback sont servis (anti
  DNS-rebinding).

Écouter ailleurs que sur loopback est possible mais affiche un avertissement : il faut alors
un proxy ou un VPN de confiance devant.

Lancer un agent, c'est exécuter du code avec les droits de l'utilisateur (le mode de permission
par défaut est `bypassPermissions`). **Helm refuse donc de lancer la moindre exécution tant qu'il
n'écoute pas sur loopback** (`RunGate::NotLoopback`) : l'avertissement au démarrage, un bandeau
sur le tableau et une note dans le formulaire de carte le disent, et aucune exécution n'est mise
en file. Sans dépôt configuré (`RunGate::NoRepo`), l'assignation d'un agent est refusée. Les
routes d'annulation et de publication, d'actualisation ou de fusion des PR passent par le même
garde `Origin` / `Sec-Fetch-Site` que les autres. L'intégration GitHub exige elle aussi un dépôt
configuré et une écoute sur loopback. Les vérifications sont des commandes locales exécutées
avec les droits du processus Helm.

## 3. Modèle de données

### Livré (migrations `0001_init` à `0003_autoincrement_comment_ids`)

```
projects 1──* board_columns 1──* cards *──* labels
                                   │      (card_labels)
                                   └──1──* comments 1──* mentions
```

- **`projects`** — `key` (préfixe des identifiants, ex. `HELM`), `name`,
  `next_card_number`. Un projet par défaut est créé ; l'interface v0.1 n'affiche que lui.
- **`board_columns`** (colonnes = statuts) — `name` (libellé affiché, modifiable), `position`,
  et **`category`** : `backlog`, `todo`, `in_progress`, `in_review`, `done`. La catégorie est
  le statut stable sur lequel l'orchestrateur raisonnera ; le nom n'est que de l'affichage.
  Colonnes par défaut : Backlog, À faire, En cours, En revue, Terminé.
- **`cards`** — `id` (`AUTOINCREMENT` : jamais réattribué, un formulaire resté ouvert sur une
  carte supprimée ne peut donc pas en atteindre une autre), `number` (séquentiel par projet,
  jamais réutilisé : `HELM-12`), `title`, `description`, `priority`, `column_id`, `position`,
  horodatages Unix.
  - **Ordre** : `position` est un rang dense (0, 1, 2…) dans la colonne, renuméroté dans la
    même transaction à chaque déplacement ou suppression. Simple, sans dérive, et le coût
    (quelques dizaines de lignes) est sans importance ici.
  - **Priorités** : entier ordonné — 0 aucune, 1 basse, 2 moyenne, 3 haute, 4 urgente.
- **`labels`** / **`card_labels`** — étiquettes par projet, uniques sans tenir compte de la
  casse, créées à la saisie et supprimées quand plus aucune carte ne les porte. `color_slot`
  (0–7) désigne une variable CSS `--label-N` : la base ne stocke aucune couleur, la charte
  reste maîtresse du rendu.

- **`comments`** — `id` (`AUTOINCREMENT`, jamais réattribué : un curseur ou un lien sur un ancien
  commentaire ne peut pas atteindre celui d'un autre), `card_id` (suppression en cascade avec la carte), `author_kind`
  (`human` | `agent` | `system`, contraint en base), `author` (nom affiché : `moi`, `claude`,
  `codex`…), `body` (source Markdown, affiché en texte brut échappé, retours à la ligne
  conservés), `created_at`. Le fil d'une carte est lu par `id` croissant. Il n'y a ni édition
  ni suppression d'un commentaire.
- **`mentions`** — `id` (`AUTOINCREMENT`), `comment_id` (cascade), `target`, `handled_at`. Extraites dans la même
  transaction que le commentaire, une ligne par cible distincte. L'ensemble des cibles est
  fermé et vit dans `src/mentions.rs` (`@claude`, `@codex`, `@moi`) : `@param` ou une adresse
  électronique (`nom@codex.com`) ne crée aucune mention, car personne ne la traiterait. La base
  ne contraint pas `target`, pour qu'ajouter un agent n'exige pas de reconstruire la table.
  `handled_at` reste `NULL` tant que l'orchestrateur n'a pas agi ; l'index partiel
  `mentions_unhandled (target, comment_id) WHERE handled_at IS NULL` fait de « ce qui attend
  l'agent X, le plus ancien d'abord » une seule requête sur l'index. Rien ne consomme encore
  les mentions : le fil les met seulement en évidence.

### Livré avec l'orchestrateur (migration `0004_agent_runs`)

- **`cards.agent`**, **`cards.model`** — l'assignation est **par carte** : un agent (`claude`, ou
  rien) et un modèle (`--model`, ou rien pour le défaut du projet dans `helm.toml`). Ni l'un ni
  l'autre n'est contraint en base (comme `mentions.target`) : les valeurs connues vivent dans
  `src/agent.rs`. Sans dépôt configuré, aucun agent ne peut être assigné.
- **`agent_runs`** (exécutions d'agent) — une ligne par lancement d'un agent sur une carte :

  | Colonne           | Sens                                                               |
  | ----------------- | ------------------------------------------------------------------ |
  | `card_id`         | Carte servie (suppression en cascade).                             |
  | `agent`, `model`, `permission_mode` | Ce avec quoi l'exécution a été lancée.           |
  | `status`          | `queued`, `running`, `succeeded`, `failed`, `cancelled`, `interrupted`. |
  | `prompt`          | Consigne envoyée (pour l'audit et la relance).                     |
  | `session_id`      | Identifiant de session rendu par le CLI, clé de la reprise.        |
  | `resumed_from`    | Exécution précédente, quand celle-ci en reprend une ; les tentatives restent distinctes. |
  | `worktree_path`, `branch` | Worktree git isolé de la carte.                            |
  | `pid`, `exit_code`| Suivi du processus.                                                |
  | `error`, `stderr` | Pourquoi l'exécution a échoué ; sortie d'erreur du CLI, à part.    |
  | `cost_usd`, `tokens_in`, `tokens_out` | Consommation, quand le CLI la rapporte.        |
  | `queued_at`, `started_at`, `finished_at`, `pushed_at` | Horodatages.                   |

  Le statut est une machine à états (`RunStatus::can_become` dans `src/runs.rs`) :
  `queued → running | cancelled`, `running → succeeded | failed | cancelled | interrupted`, et
  rien ne sort d'un état final (un nouvel essai est une nouvelle exécution). La base garantit
  deux invariants : au plus une exécution active (`queued` ou `running`) par carte, par un
  index unique partiel ; et une exécution n'est `succeeded` que si sa branche est poussée
  (`pushed_at`), si bien qu'un échec de `git push` ne peut pas être enregistré comme un succès.
  Une carte qui porte une exécution active ne se supprime pas.

- **`agent_events`** — journal append-only d'une exécution : `run_id`, `seq` (dense par
  exécution), `kind` (`init`, `message`, `tool_use`, `tool_result`, `result`, `system`,
  `notice`, `error`, `malformed`), `summary` (la ligne affichée sur la carte), `payload` (ligne
  JSON brute du CLI, intacte), `created_at`. C'est le fil d'activité affiché sur la carte.

### Livré avec les vérifications et GitHub (migration `0005_delivery`)

- `run_verifications` contient un résultat global par `run_id`, lié à `commit_sha`.
  Les états sont `running`, `passed`, `failed`, `skipped` et `interrupted`.
  L'absence de commandes produit `skipped`, affiché « Non exécutées ».
- `run_checks` contient chaque commande dans son ordre d'exécution : `run_id`, `position`,
  `command`, `status`, `stdout`, `stderr`, `exit_code` et `error`. Les états sont `pending`,
  `running`, `passed`, `failed` et `interrupted`. Un contrôle réussi exige un code de retour nul.
  Après un échec, les commandes restantes restent en attente et ne sont pas exécutées.
- `card_pull_requests` conserve un instantané par carte : `card_id`, `run_id`, `repository`,
  `number`, `expected_base`, `snapshot` JSON, `refreshed_at` et `error`. Le couple dépôt et
  numéro de PR est unique. L'instantané contient l'état, le commit, les branches, les contrôles
  CI et les conditions de fusion renvoyés par GitHub. Une actualisation ou une reprise conserve
  la base attendue de la même PR. Si la lecture de GitHub échoue, Helm garde le dernier instantané
  et enregistre l'erreur.

Les résultats disparaissent en cascade avec l'exécution ou la carte. Une PR peut rester liée
à une exécution précédente après une relance : l'interface l'indique et bloque sa fusion.

### Prévu (migrations suivantes)

- **`cards.parent_id`** — sous-cartes : un agent découpe son travail ou délègue en créant des
  cartes filles.

## 4. Cycle de vie d'une carte

```
            créer                planifier              démarrer
  (rien) ─────────▶ Backlog ───────────────▶ À faire ─────────────▶ En cours
                                                 ▲                      │
                                    retours      │                      │ travail terminé
                                 ┌───────────────┘                      ▼
                                 │                                  En revue
                                 └──────────────────────────────────────┤
                                                                        │ accepté
                                                                        ▼
                                                                     Terminé
```

**Livré.** Une carte naît en bas d'une colonne (ajout rapide, titre seul), s'enrichit dans le
dialogue d'édition (description, priorité, étiquettes, colonne) et se déplace librement :
glisser-déposer, `Alt` + flèches au clavier, ou liste « Colonne » du formulaire. Aucune
transition de colonne n'est interdite. Pendant une opération GitHub, modifier, déplacer ou
supprimer la carte est temporairement refusé pour éviter une course avec la livraison.
Le dialogue d'édition porte aussi le fil de commentaires de la carte et un formulaire pour
en ajouter (auteur `moi`). La suppression est définitive
(confirmation dans le dialogue, ou page de confirmation sans JavaScript).

**Livré, avec l'orchestrateur.** Les colonnes gardent leur liberté pour l'humain, mais leur
catégorie déclenche et reflète le travail des agents :

| Transition                         | Qui                | Effet                                                     |
| ---------------------------------- | ------------------ | --------------------------------------------------------- |
| → `todo` avec un agent assigné     | humain             | L'exécution est mise en file (`queued`). Sortir la carte de `todo` annule l'exécution en file. |
| `todo` → `in_progress`             | orchestrateur      | Worktree créé, processus lancé (`running`).               |
| `in_progress` → `in_review`        | orchestrateur      | L'agent a terminé avec succès et la branche est poussée sur `origin`. |
| `in_progress` (reste)              | orchestrateur      | Échec, annulation ou interruption : la carte ne bouge pas, l'erreur est dans l'activité de la carte et dans son fil (commentaire du système, qui n'enregistre jamais de mention). |
| `in_review` → `todo`               | humain             | Nouvelle exécution dans une nouvelle session. |
| carte non terminée → `in_progress` | reprise explicite  | Le commentaire de correction met une nouvelle exécution en file ; le superviseur reprend la session existante quand une place se libère. |
| `in_review` → `done`               | humain             | Travail accepté ; le worktree peut être nettoyé (à la main pour l'instant). |
| carte → `done`                     | livraison GitHub   | GitHub confirme la fusion de la PR de la dernière exécution, en état `succeeded`, avec le commit vérifié et les branches attendues. |

Pour un déplacement, seule l'*entrée* dans une colonne `todo` déclenche : enregistrer le formulaire
d'une carte déjà là, ou la réordonner dans la colonne, ne relance rien. Le formulaire de reprise
est une action distincte. Ajouter un commentaire ordinaire ou une `@mention` ne lance aucun agent.

L'agent ne décide pas de la fusion. L'humain la demande dans Helm ou sur GitHub, puis Helm
actualise la carte après confirmation. Déplacer manuellement une carte vers `done` reste
possible et ne déclenche aucune fusion.

## 5. Orchestrateur (livré)

### Modèle d'exécution

Par défaut, un agent est un **processus sans interface**, enfant de Helm. Livré : Claude,
dont la consigne passe par l'entrée standard (elle peut contenir tout le fil de commentaires).
La reprise de session Claude est livrée. Codex reste prévu.

```
claude -p --output-format stream-json --verbose --permission-mode <mode> [--model <m>]   # livré
claude -p … --resume <session_id>                       # livré
codex exec --json "<consigne>"                          # prévu ; reprise : codex exec resume <session_id>
```

- **Un worktree git par carte.** `git worktree add <racine>/<clé>-<numéro> -b helm/<clé>-<numéro>`
  depuis la branche par défaut du dépôt (celle de `origin/HEAD` si elle est connue, sinon
  `main`, `master`, ou la branche extraite) ; le processus y est lancé (`cwd`), dans son propre
  groupe de processus, tué en entier à l'annulation. Les agents ne se marchent pas
  dessus et le dépôt principal reste intact. Le worktree survit à l'exécution (revue, reprise)
  et son nettoyage reste manuel.
- **Événements structurés sur la carte.** La sortie standard est lue ligne à ligne (un objet
  JSON par ligne), normalisée par un adaptateur propre à chaque CLI, puis insérée dans
  `agent_events`. Chaque insertion publie un événement SSE : la carte affiche l'activité en
  direct, et l'historique complet reste consultable après coup. La sortie d'erreur est
  conservée à part pour le diagnostic (`agent_runs.stderr`, 256 Kio au plus). Une ligne de plus
  de 1 Mio est tronquée à la lecture et signalée dans le journal, sans interrompre l'exécution. Les nouvelles
  lignes sont annoncées aux navigateurs au plus quatre fois par seconde par exécution.
- **Reprise par identifiant de session.** Le `session_id` émis par le CLI est enregistré dès
  qu'il apparaît. Le formulaire de correction crée une nouvelle exécution qui reprend cette
  session selon le contrat de [`claude --resume`](https://code.claude.com/docs/en/cli-reference).
  Les conditions et le stockage sont détaillés ci-dessous.
- **Supervision.** Les processus sont lancés avec `tokio::process` et attendus de façon
  asynchrone ; une limite de concurrence configurable borne le nombre d'agents simultanés.
  Un arrêt normal de Helm (SIGINT, SIGTERM) arrête chaque agent comme une annulation (SIGTERM au
  groupe, puis SIGKILL après 3 s) et marque l'exécution `interrupted`. Un agent encore actif
  après `agents.run_timeout_minutes` est arrêté de la même façon et l'exécution échoue.
  Au démarrage, toute exécution encore `running` en base passe à `interrupted` — jamais
  relancée en silence ; si son processus existe encore, l'erreur le dit (Helm ne peut plus le
  superviser et ne tue pas un pid qu'il ne peut pas identifier). Pour la même raison, une
  nouvelle exécution est refusée tant que le groupe de processus de la dernière exécution de la
  carte ayant lancé un agent existe encore, si celle-ci a été interrompue (une exécution plus
  récente qui a lancé un agent a passé ce contrôle : les pid plus anciens ne comptent plus, un
  autre programme a pu les réutiliser). Le message dit de vérifier ce qu'est ce processus avant de
  l'arrêter, puis donne la commande. Une seule exécution active par
  carte (index unique partiel).
- **Fin de travail : pousser la branche.** Après un succès de l'agent, Helm demande à
  `origin` où en est la branche (`git ls-remote`, jamais la copie locale `origin/<branche>`, qui peut
  être périmée) et la compare au worktree. Si la branche porte des commits que `origin` n'a pas,
  qu'ils viennent de cette exécution ou d'une précédente restée sans push, Helm les pousse
  vers `origin`, sans forcer. Si `origin` a déjà tout (ou n'a pas
  la branche et que le worktree n'ajoute rien à la branche par défaut), l'exécution échoue : « rien à
  pousser ». Si `origin` a des commits que le worktree n'a pas (historique réécrit, travail poussé
  d'ailleurs), elle échoue aussi, sans push. `origin` injoignable : elle échoue. Un push refusé
  ou impossible fait échouer l'exécution : la base refuse un `succeeded` sans `pushed_at`.
  Avec des vérifications configurées ou GitHub activé, Helm pousse le SHA vérifié exact et
  exige un worktree propre. Sans ces options, les modifications non commitées restent dans
  le worktree et sont signalées dans le journal, comme auparavant.

Un trait `AgentAdapter` isole ce qui diffère entre CLI : construire la commande (lancement et
reprise), reconnaître l'identifiant de session, traduire chaque ligne en événement normalisé,
détecter la fin et son issue. Ajouter un agent, c'est écrire un adaptateur.

### Reprise après des retours

`POST /cards/{id}/resume` reçoit le texte de correction et `source_run_id`, l'exécution affichée
lors de la demande. L'orchestrateur vérifie à nouveau que cette exécution est la dernière,
qu'elle est terminée et que la carte n'est pas dans `done`. L'agent assigné doit correspondre
à celui de la session. La même restriction loopback et la même réservation par carte que pour
les autres lancements s'appliquent.

Le commentaire humain et l'exécution `queued` sont enregistrés dans une seule transaction.
Un formulaire périmé, une demande répétée ou une autre exécution active ne laisse donc pas de
commentaire sans reprise associée. La nouvelle ligne pointe vers la précédente via
`resumed_from`, déjà présent dans la migration `0004`. Elle conserve le modèle et le mode de
permission précédents, ainsi qu'une consigne figée lors de la demande ; les tentatives et leurs
vérifications restent distinctes. Une reprise annulée avant d'avoir reçu
un identifiant de session peut remonter cette chaîne pour retrouver la session antérieure.
La mise en file n'exige pas de déplacer la carte vers `todo` ; le superviseur la passe en
`in_progress` lorsqu'il prend l'exécution.

La reprise réutilise le worktree enregistré et vérifie qu'il appartient au dépôt et porte
la branche attendue. Elle ne recrée pas un répertoire disparu et ne supprime pas de travail
non commité. Le contrôle des processus orphelins reste applicable. Un historique de session
indisponible pour Claude produit un échec visible, sans nouvelle session de secours. Une
entrée explicite dans `todo` conserve son comportement de lancement neuf.

Après la reprise, les vérifications, le push et la publication suivent le même parcours que
pour un lancement neuf. Une PR enregistrée reste attachée à son dépôt, son numéro et sa base ;
la publication relit cette PR précise et refuse un état fermé, fusionné ou une base modifiée.
Les résultats de l'exécution précédente ne permettent plus de fusionner dès qu'une nouvelle
exécution existe.

### Vérifications locales avant publication

`[checks]` configure une liste `commands` et un `timeout_minutes`, de 10 minutes par commande
par défaut. Après le succès de l'agent, `checks::execute` passe chaque chaîne à `sh -c` dans le
worktree. Les commandes s'exécutent dans l'ordre. Un échec ou un timeout bloque les suivantes
et le push. L'annulation ou l'arrêt de Helm interrompt les contrôles encore actifs.

Avec des commandes ou GitHub activé, Helm vérifie la branche attendue, la propreté du worktree
et le SHA avant les contrôles, après eux et juste avant le push. Le résultat global ne devient
`passed` que si toutes les commandes réussissent sans changement du commit ni des fichiers.
Sans commande, il reste `skipped`, même si Helm a validé le commit et la propreté pour GitHub.

`process::capture` limite séparément stdout et stderr à 1 Mio et signale une troncature.
Chaque commande a son propre groupe de processus. Un dépassement de délai ou une annulation
arrête le groupe pour ne pas laisser de commande enfant continuer les vérifications.

### Publication et fusion des PR GitHub

Sans `[github]`, l'intégration est désactivée. `github.repository` désigne `owner/repo` sur
github.com. `github.command` vaut `gh` par défaut ; ce CLI doit être installé et authentifié
par l'utilisateur. Les appels précisent le dépôt et l'hôte, avec un délai de 120 secondes par
commande. La configuration et un exemple figurent dans le [README](../README.md).

Après un push réussi, `Delivery::publish` crée ou retrouve la PR ouverte correspondant aux
branches attendues. Une PR en brouillon devient prête à relire. La base vient de la résolution
de la branche par défaut du dépôt dans `git.rs`. Lors de la création, le corps transmis sur
stdin indique le SHA et les résultats de Helm. Une PR réutilisée conserve son corps existant.
L'identité et l'URL de la PR sont validées avant stockage.

Une reprise avec une PR déjà liée conserve cette identité et sa base attendue même quand
`run_id` change. Helm vérifie la PR enregistrée par numéro au lieu de rechercher une autre PR
sur la branche. Une fermeture, une fusion ou un changement de base est signalé ; aucune PR
de remplacement n'est créée automatiquement.

Une erreur ou une annulation pendant la publication de la PR ne remet pas en cause le push
réussi. L'exécution se termine en `succeeded`, avec un commentaire expliquant le problème.
`Delivery::retry` republie le SHA enregistré dans les vérifications de la dernière exécution,
qui doit être réussie, sans relancer l'agent ni les commandes. Le bouton « Créer la PR / Réessayer » appelle
cette opération.

Les opérations GitHub et la mise en file partagent une réservation par carte. Une exécution
active bloque l'actualisation et la fusion manuelles. Une livraison déjà en cours bloque une
autre livraison et les modifications de la carte, afin qu'une fusion ne vise pas une exécution
remplacée entre-temps.

`Delivery::merge` relit GitHub et exige une dernière exécution en état `succeeded`, des vérifications
`passed` ou `skipped`, le même SHA, la même branche et la base attendue. Le client refuse les
brouillons, les CI en cours, échouées ou inconnues, les revues bloquantes et les états dont
GitHub ne confirme pas la fusion possible. Une liste vide de contrôles CI est « Aucun contrôle »,
distincte de « Réussis » ; elle n'est pas bloquante à elle seule.

La fusion passe par `gh pr merge --squash --match-head-commit <SHA>`, sans contournement
administrateur. Un code de retour nul ne suffit pas : une nouvelle lecture doit confirmer
`MERGED`, le commit et la base attendus. Helm passe alors la carte à `done` seulement si la PR
correspond toujours à la dernière exécution de la carte, qui doit être réussie, à son commit
vérifié et à sa branche.

L'actualisation manuelle et la boucle d'arrière-plan, cadencée toutes les 30 secondes,
reconnaissent aussi les fusions faites directement sur GitHub. La boucle traite séquentiellement
les cartes non terminées. Si la lecture de GitHub échoue, Helm conserve le dernier instantané
et enregistre l'erreur.

### tmux : prévu, pour regarder et reprendre la main

Le mode sans interface est la règle. tmux n'est qu'une **option par carte**, pour le cas où
l'humain veut voir l'agent travailler ou intervenir : l'orchestrateur lance alors le CLI
interactif dans une session tmux nommée d'après la carte, à laquelle on s'attache à la main
(`tmux attach -t helm-HELM-12`). Dans ce mode, les événements structurés ne sont plus garantis :
la carte porte le lien vers la session et son état, pas un fil détaillé. Helm ne dépend jamais
de tmux pour fonctionner.

### Communication entre agents : par le tableau, jamais par le terminal

Un agent n'écrit **jamais** dans le terminal ni dans l'entrée standard d'un autre. Toute
coordination passe par des objets du tableau, visibles et historisés :

- **Commentaires** — un agent rend compte, pose une question, laisse une note de passation.
  Le stockage et l'affichage sont livrés (section 3) ; seul l'humain écrit pour l'instant.
- **Mentions** — `@codex` dans un commentaire crée une mention non traitée (livré). Leur
  traitement automatique reste **prévu** : l'orchestrateur transformerait une mention en
  exécution ou reprise avec le fil comme contexte, et `@moi` bloquerait la carte en attente
  de l'humain. Aujourd'hui, une reprise exige le formulaire explicite de correction.
- **Sous-cartes** — pour déléguer, un agent crée une carte fille ; elle suit son propre cycle
  de vie, dans son propre worktree. La carte mère voit l'avancement de ses filles.

Les agents agissent sur le tableau par une **interface dédiée** (sous-commandes `helm` ou API
HTTP locale — voir décisions ouvertes), jamais par accès direct à la base. Conséquences
recherchées : tout échange est auditable, un humain peut s'insérer dans n'importe quelle
conversation, et remplacer un agent par un autre ne change pas le protocole.

## 6. Interface et style (livré)

- **Variables CSS centralisées.** `assets/tokens.css` est l'unique endroit où figurent des
  valeurs : couleurs, typographie, espacements, rayons, ombres, anneau de focus, dimensions,
  durées. `assets/app.css` ne lit que des `var(--…)` ; un test échoue si une couleur y est
  écrite en dur. Brancher la charte = remplacer les valeurs de `tokens.css`.
- **Thème clair/sombre.** Chaque couleur est déclarée une seule fois avec
  `light-dark(clair, sombre)` ; `color-scheme` suit le système, et le bouton de thème force
  `data-theme="light|dark"` sur `<html>` (mémorisé en `localStorage`).
- **Clavier.** Flèches pour naviguer entre cartes et colonnes, `Alt` + flèches pour déplacer,
  `Entrée` pour ouvrir, `N` pour une nouvelle carte, `Échap` pour fermer. Focus toujours
  visible (`:focus-visible`), lien d'évitement, région `aria-live` annonçant les déplacements.
- **Activité de l'agent.** Le dialogue de la carte affiche la dernière exécution : statut, branche,
  session, durée, coût, jetons, journal des 200 derniers événements (le bruit du CLI est
  enregistré mais masqué), sortie d'erreur et consigne. `GET /cards/{id}/activity` en rend le
  fragment ; le script le recharge à chaque événement SSE sans toucher au formulaire. Sans
  JavaScript, la page de la carte porte le même panneau et le bouton d'annulation est un
  formulaire.
- **Retours et reprise.** « Demander une correction » contient les « Retours pour l'agent » et
  le bouton « Reprendre avec ces retours ». Le serveur revalide l'exécution source au POST.
  Une réussite redirige vers la carte sans JavaScript ; avec le script, le panneau est rechargé.
  Le script conserve un brouillon de correction lors des actualisations SSE et après un échec
  de l'envoi. Si une autre exécution devient la dernière pendant la saisie, il bloque le bouton
  et invite à revoir la demande, au lieu de changer silencieusement l'exécution à reprendre.
- **Vérifications et PR.** Le même panneau affiche le commit vérifié, chaque commande et ses
  sorties, le lien de PR, son état, la CI et la dernière actualisation. Les actions sont des
  POST sous `/cards/{id}/pull-request/`, avec les suffixes `publish`, `refresh` et `merge`. Le serveur valide
  l'état au moment de l'action ; le bouton de fusion est masqué si l'instantané ne l'autorise
  pas. Sans JavaScript, une action réussie redirige vers la carte ; avec le script, elle
  renvoie `204` et le panneau est rechargé.
- **Sans JavaScript**, créer, modifier, déplacer (liste « Colonne »), supprimer et commenter
  restent possibles par envoi de formulaire classique. Les heures des commentaires sont alors
  affichées en UTC ; le script les convertit dans le fuseau du navigateur.

## 7. Décisions ouvertes

1. **Interface des agents avec le tableau** — sous-commandes du binaire (`helm comment`,
   `helm card create`…), API HTTP JSON locale, ou serveur MCP ? Le CLI est le plus simple à
   donner à `claude -p` et `codex exec` ; MCP est plus riche mais ajoute une surface.
2. **Permissions des agents** — décidé pour la première tranche : autonomie complète
   (`bypassPermissions`), réglable globalement dans `helm.toml`
   (`agents.claude.permission_mode`). Reste ouvert : un réglage par projet ou par carte, et le
   bac à sable de Codex.
3. **Authentification** — contournée, pas résolue : Helm refuse de lancer des exécutions hors
   loopback. Un jeton local reste nécessaire pour écouter ailleurs.
4. **Projets et dépôts** — un projet = un dépôt (`project.repo`), worktrees sous
   `project.worktree_root`. Reste ouvert : qui nettoie les worktrees (aucun nettoyage
   automatique pour l'instant).
5. **Assignation** — un agent par carte, choisi par l'humain (champ du formulaire). Reste
   ouvert : plusieurs rôles (auteur, relecteur) et les règles de projet.
6. **Fin de travail** : livré. L'agent s'arrête au commit. Helm exécute les contrôles configurés,
   pousse la branche et publie une PR si GitHub est activé. La fusion est explicite et la
   confirmation GitHub conditionne le passage automatique à Terminé.
7. **Rétention des événements** — les flux `stream-json` sont volumineux : tout garder,
   compacter après N jours, ou ne conserver que les événements significatifs ?
8. **Budget** — plafond de coût ou de durée par exécution, et comportement à l'atteinte.
9. **Interface multi-projets** — le schéma les prévoit, l'interface n'en montre qu'un ; les
   routes devront porter la clé du projet.
10. **Colonnes personnalisables** — création, renommage et réordonnancement depuis
    l'interface (les catégories garantissent déjà que l'orchestrateur n'en dépend pas).
11. **Journalisation** — la v0.1 écrit sur la sortie d'erreur ; adopter `tracing` quand
    l'orchestrateur aura besoin de journaux structurés.
12. **Ordre des cartes** — le rang dense suffit aujourd'hui ; à revoir (rang fractionnaire)
    seulement si des agents réordonnent massivement en parallèle.
