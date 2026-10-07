# Architecture de Helm

Helm est un tableau kanban auto-hébergé pour un développeur solo, dans l'esprit de Symphony
(un tableau qui orchestre des agents de code) croisé avec Linear et ClickUp. Les agents sont
des CLI (`claude -p`, `codex exec`) ; le tableau est à la fois l'outil de suivi et le bus par
lequel ils se coordonnent.

Ce document décrit ce qui existe (v0.1 : le kanban) et la conception de ce qui vient
(l'orchestrateur). Chaque section précise son état : **livré** ou **prévu**.

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
| `store.rs`      | Modèle du tableau et opérations sur les cartes (synchrone, testé seul).     |
| `routes.rs`     | Pages, formulaires, flux SSE, garde-fous de requête.                        |
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
- **Pas de framework front.** Environ 350 lignes de JavaScript sans dépendance : glisser-déposer
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

### Sécurité du mode local

Sans authentification, tout ce qui atteint le port peut modifier le tableau. Deux garde-fous
empêchent une page web ouverte dans le même navigateur de le piloter :

- les écritures dont l'en-tête `Origin` ne correspond pas à `Host`, ou marquées
  `Sec-Fetch-Site: cross-site`, sont refusées (anti-CSRF) ;
- quand l'écoute est sur loopback, seuls les noms d'hôte loopback sont servis (anti
  DNS-rebinding).

Écouter ailleurs que sur loopback est possible mais affiche un avertissement : il faut alors
un proxy ou un VPN de confiance devant. Ce point devient critique dès que le tableau lancera
des agents (voir les décisions ouvertes).

## 3. Modèle de données

### Livré (migration `0001_init`)

```
projects 1──* board_columns 1──* cards *──* labels
                                   (card_labels)
```

- **`projects`** — `key` (préfixe des identifiants, ex. `HELM`), `name`,
  `next_card_number`. Un projet par défaut est créé ; l'interface v0.1 n'affiche que lui.
- **`board_columns`** (colonnes = statuts) — `name` (libellé affiché, modifiable), `position`,
  et **`category`** : `backlog`, `todo`, `in_progress`, `in_review`, `done`. La catégorie est
  le statut stable sur lequel l'orchestrateur raisonnera ; le nom n'est que de l'affichage.
  Colonnes par défaut : Backlog, À faire, En cours, En revue, Terminé.
- **`cards`** — `number` (séquentiel par projet, jamais réutilisé : `HELM-12`), `title`,
  `description`, `priority`, `column_id`, `position`, horodatages Unix.
  - **Ordre** : `position` est un rang dense (0, 1, 2…) dans la colonne, renuméroté dans la
    même transaction à chaque déplacement ou suppression. Simple, sans dérive, et le coût
    (quelques dizaines de lignes) est sans importance ici.
  - **Priorités** : entier ordonné — 0 aucune, 1 basse, 2 moyenne, 3 haute, 4 urgente.
- **`labels`** / **`card_labels`** — étiquettes par projet, uniques sans tenir compte de la
  casse, créées à la saisie et supprimées quand plus aucune carte ne les porte. `color_slot`
  (0–7) désigne une variable CSS `--label-N` : la base ne stocke aucune couleur, la charte
  reste maîtresse du rendu.

### Prévu (migrations suivantes)

- **`cards.parent_id`** — sous-cartes : un agent découpe son travail ou délègue en créant des
  cartes filles.
- **`comments`** — `card_id`, `author_kind` (`human` | `agent` | `system`), `author`
  (nom d'agent ou d'utilisateur), `body` (Markdown), `created_at`. Les **mentions**
  (`@claude`, `@codex`, `@moi`) sont extraites à l'écriture dans **`mentions`**
  (`comment_id`, `target`, `handled_at`) pour que l'orchestrateur trouve en une requête ce qui
  attend une réponse.
- **`agent_runs`** (exécutions d'agent) — une ligne par lancement d'un agent sur une carte :

  | Colonne           | Sens                                                               |
  | ----------------- | ------------------------------------------------------------------ |
  | `card_id`         | Carte servie.                                                      |
  | `agent`           | `claude` ou `codex` (extensible).                                  |
  | `status`          | `queued`, `running`, `succeeded`, `failed`, `cancelled`, `interrupted`. |
  | `session_id`      | Identifiant de session rendu par le CLI, clé de la reprise.        |
  | `resumed_from`    | Exécution précédente, quand celle-ci en reprend une.               |
  | `worktree_path`, `branch` | Worktree git isolé de la carte.                            |
  | `prompt`          | Consigne envoyée (pour l'audit et la relance).                     |
  | `pid`, `exit_code`| Suivi du processus.                                                |
  | `cost_usd`, `tokens_in`, `tokens_out` | Consommation, quand le CLI la rapporte.        |
  | `started_at`, `finished_at` | Horodatages.                                             |

- **`agent_events`** — journal append-only d'une exécution : `run_id`, `seq`, `kind`
  (`message`, `tool_use`, `tool_result`, `error`, `result`…), `payload` (JSON brut du CLI),
  `created_at`. C'est le fil d'activité affiché sur la carte.

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
transition n'est interdite — c'est l'outil d'une seule personne. La suppression est définitive
(confirmation dans le dialogue).

**Prévu, avec l'orchestrateur.** Les colonnes gardent leur liberté pour l'humain, mais leur
catégorie déclenche et reflète le travail des agents :

| Transition                         | Qui                | Effet                                                     |
| ---------------------------------- | ------------------ | --------------------------------------------------------- |
| → `todo` avec un agent assigné     | humain             | L'exécution est mise en file (`queued`).                  |
| `todo` → `in_progress`             | orchestrateur      | Worktree créé, processus lancé (`running`).               |
| `in_progress` → `in_review`        | orchestrateur      | L'agent a terminé avec succès ; résumé et diff sur la carte. |
| `in_progress` (reste)              | orchestrateur      | Échec ou interruption : la carte ne bouge pas, l'erreur est commentée, l'humain décide. |
| `in_review` → `todo`               | humain             | Retours en commentaire ; l'exécution suivante **reprend la session**. |
| `in_review` → `done`               | humain             | Travail accepté ; le worktree peut être nettoyé.          |

Un agent ne déplace jamais une carte vers `done` : la revue reste humaine.

## 5. Orchestrateur (prévu, non livré)

### Modèle d'exécution

Par défaut, un agent est un **processus sans interface**, enfant de Helm :

```
claude -p "<consigne>" --output-format stream-json      # reprise : --resume <session_id>
codex exec --json "<consigne>"                          # reprise : codex exec resume <session_id>
```

- **Un worktree git par carte.** `git worktree add <racine>/<clé>-<numéro> -b helm/<clé>-<numéro>`
  depuis le dépôt du projet ; le processus y est lancé (`cwd`). Les agents ne se marchent pas
  dessus et le dépôt principal reste intact. Le worktree survit à l'exécution (revue, reprise)
  et n'est supprimé qu'une fois la carte terminée ou archivée.
- **Événements structurés sur la carte.** La sortie standard est lue ligne à ligne (un objet
  JSON par ligne), normalisée par un adaptateur propre à chaque CLI, puis insérée dans
  `agent_events`. Chaque insertion publie un événement SSE : la carte affiche l'activité en
  direct, et l'historique complet reste consultable après coup. La sortie d'erreur est
  conservée à part pour le diagnostic.
- **Reprise par identifiant de session.** Le `session_id` émis par le CLI est enregistré dès
  qu'il apparaît. Relancer après des retours de revue, un échec ou un redémarrage de Helm
  crée une **nouvelle** exécution (`resumed_from`) qui reprend la même session dans le même
  worktree : le contexte de l'agent est conservé, l'historique des tentatives aussi.
- **Supervision.** Les processus sont lancés avec `tokio::process` et attendus de façon
  asynchrone ; une limite de concurrence configurable borne le nombre d'agents simultanés.
  Au démarrage, toute exécution encore `running` en base dont le processus n'existe plus
  passe à `interrupted` — reprenable, jamais relancée en silence.

Un trait `AgentAdapter` isole ce qui diffère entre CLI : construire la commande (lancement et
reprise), reconnaître l'identifiant de session, traduire chaque ligne en événement normalisé,
détecter la fin et son issue. Ajouter un agent, c'est écrire un adaptateur.

### tmux : optionnel, pour regarder et reprendre la main

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
- **Mentions** — `@codex` dans un commentaire crée une mention non traitée ; l'orchestrateur
  la transforme en exécution (ou en reprise) de l'agent visé, sur cette carte, avec le fil de
  commentaires comme contexte. `@moi` bloque la carte en attente de l'humain.
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
- **Sans JavaScript**, créer, modifier, déplacer (liste « Colonne ») et supprimer restent
  possibles par envoi de formulaire classique.

## 7. Décisions ouvertes

1. **Interface des agents avec le tableau** — sous-commandes du binaire (`helm comment`,
   `helm card create`…), API HTTP JSON locale, ou serveur MCP ? Le CLI est le plus simple à
   donner à `claude -p` et `codex exec` ; MCP est plus riche mais ajoute une surface.
2. **Permissions des agents** — quels outils et quel niveau d'autonomie par défaut
   (`--permission-mode`, bac à sable de Codex) ? Réglage global, par projet ou par carte ?
3. **Authentification** — nécessaire dès que Helm lance des processus : une requête acceptée
   devient de l'exécution de code. Jeton local, ou rester strictement sur loopback ?
4. **Projets et dépôts** — un projet = un dépôt git ? Où vivent les worktrees
   (`~/.local/share/helm/worktrees`, à côté du dépôt) et qui les nettoie ?
5. **Assignation** — un agent par carte, ou plusieurs rôles (auteur, relecteur) ? Qui choisit
   l'agent : l'humain, une étiquette, une règle de projet ?
6. **Fin de travail** — l'agent ouvre-t-il une PR, pousse-t-il une branche, ou s'arrête-t-il
   au commit local dans le worktree ?
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
