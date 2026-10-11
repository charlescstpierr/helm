# Héberger Helm avec Docker et Cloudflare

Ce guide donne accès à Helm depuis un téléphone ou un ordinateur, en HTTPS, après une
connexion Cloudflare Access. Helm écoute uniquement sur `127.0.0.1:7878` dans le conteneur.
`cloudflared` et Helm partagent le même espace réseau. Aucun port n'est publié sur le serveur.

**Helm n'a aucune authentification. Sans Cloudflare Access, n'importe qui ayant l'adresse
peut modifier le tableau et lancer du code sur votre serveur. Créez la protection Access
avant la route publique du tunnel.**

Vous aurez besoin de Docker avec Docker Compose, d'un domaine actif dans votre compte
Cloudflare, de votre courriel, d'un compte Claude Code et d'un compte GitHub autorisé à pousser
dans le dépôt cible. Docker est déjà installé sur le serveur inspecté.

## 1. Préparer les fichiers

Depuis le dossier du dépôt Helm qui contient `docker-compose.yml` :

```sh
cp .env.example .env
chmod 600 .env
cp deploy/helm.toml.example deploy/helm.toml
```

Ouvrez `.env` avec un éditeur, par exemple `nano .env`. Remplacez les exemples par vos valeurs :

```dotenv
TUNNEL_TOKEN=le_token_copie_depuis_cloudflare
GH_TOKEN=le_jeton_github_fine_grained_du_depot_cible
HELM_PUBLIC_HOST=helm.votre-domaine.com
TARGET_REPO=./deploy/repo
```

Le hostname n'a ni `https://`, ni port, ni chemin, ni joker. Il doit correspondre exactement
à celui déclaré dans Cloudflare. Sans cette valeur, seuls les noms d'hôte locaux sont acceptés.
Ne publiez jamais `.env`. Il est ignoré par Git et exclu du contexte de build Docker.
Évitez de partager la sortie de `docker compose config` ou de `docker inspect`, qui peut
contenir les tokens. Pour contrôler le fichier sans afficher les valeurs, utilisez
`docker compose config --quiet`.

## 2. Préparer le dépôt sur lequel travailleront les agents

Utilisez un **clone dédié**, avec son propre dossier `.git`, et un remote `origin` en HTTPS.
Un worktree de votre clone habituel a des liens vers des chemins de l'hôte qui ne sont pas
disponibles dans le conteneur. Évitez aussi de partager le clone dédié avec un autre outil
qui y crée des worktrees : Git y enregistrera les chemins internes `/repo` et `/worktrees`.

Pour tester avec Helm lui-même :

```sh
git clone https://github.com/charlescstpierr/helm.git deploy/repo
```

Pour un autre projet, remplacez cette URL. Pour un dépôt privé, clonez-le avec votre connexion
GitHub sur le serveur. `TARGET_REPO` peut aussi être le chemin absolu d'un clone dédié existant.

Dans `deploy/helm.toml`, gardez les chemins internes :

```toml
[project]
repo = "/repo"
worktree_root = "/worktrees"

[github]
repository = "charlescstpierr/helm" # à remplacer pour un autre dépôt
command = "gh"
```

Remplacez bien `owner/repo` dans le fichier exemple. Gardez la section `[checks]` existante :
`commands = []` signifie « Non exécutées ». Pour vérifier du code Helm, vous pouvez ensuite
remplacer cette liste par :

```toml
commands = [
    "cargo fmt --check",
    "cargo clippy --all-targets --locked -- -D warnings",
    "cargo test --locked",
]
```

L'image inclut Node 24 LTS, Claude Code, GitHub CLI 2.102.0, Git et Rust avec rustfmt et Clippy,
ainsi qu'un compilateur C pour SQLite. Pour un autre langage, adaptez `PROJECT_PACKAGES`
et la section des outils Rust dans le `Dockerfile` avant de reconstruire. Les vérifications
s'exécutent dans le conteneur, pas sur l'hôte.

Le compte `helm` utilise les identifiants Linux UID/GID `1000:1000`, comme l'utilisateur
`ubuntu` de ce serveur. Le clone dédié doit lui être accessible en écriture. Sur un autre
serveur, ajustez ces identifiants dans le Dockerfile ou les droits du clone dédié ; ne changez
pas les droits de vos autres dépôts. Ne montez ni le socket Docker ni tout le HOME de l'hôte.

## 3. Configurer Cloudflare, dans cet ordre

Les libellés du tableau de bord évoluent. Les chemins historiques Zero Trust > Networks >
Tunnels et Access > Applications peuvent maintenant apparaître sous Networking > Tunnels
et Zero Trust > Access controls > Applications.

1. Si votre domaine n'est pas encore actif dans Cloudflare, ajoutez-le à votre compte et
   suivez les instructions pour changer ses serveurs DNS chez votre registraire. Attendez
   qu'il soit actif.
2. Dans Zero Trust > Integrations > Identity providers, ajoutez **One-time PIN** s'il n'est
   pas déjà disponible. Cette méthode vous envoie un code de connexion par courriel.
3. Dans Zero Trust > Access controls > Applications, choisissez Create new application,
   puis Self-hosted and private, ou Self-hosted selon l'interface. Nommez-la `Helm` et ajoutez
   le hostname public exact, par exemple `helm.votre-domaine.com`. Laissez le chemin vide
   pour protéger **toute** l'application, y compris les formulaires, les assets et `/events`.
4. Ajoutez une politique avec l'action **Allow**, la règle **Include > Emails** et **votre
   unique adresse courriel exacte**. Sélectionnez One-time PIN comme méthode de connexion
   de l'application et enregistrez celle-ci. N'ajoutez pas de politique Bypass, Everyone,
   « Emails ending in », ou « Include > Login Methods > One-time PIN » : ces règles
   élargiraient l'accès. Les utilisateurs qui ne correspondent pas à l'Allow sont refusés
   par défaut ; aucune politique Block Everyone supplémentaire n'est nécessaire.
5. Dans Zero Trust > Networks > Tunnels, ou Networking > Tunnels, créez un tunnel de type
   **Cloudflared**, nommé `helm`. Dans les instructions Docker du connecteur, copiez
   seulement le token et placez-le dans `.env` à la ligne `TUNNEL_TOKEN`. N'exécutez pas
   la commande proposée par Cloudflare sur l'hôte : Compose lance le connecteur.
6. Dans le tunnel, ajoutez une route **Public hostname** ou **Routes > Add route > Published
   application**, selon l'interface. Utilisez le même hostname que l'application Access,
   sans restriction de chemin. Service : **HTTP**, adresse **127.0.0.1:7878**, soit
   `http://127.0.0.1:7878`. Ne remplacez pas l'en-tête HTTP Host : Helm doit recevoir votre
   domaine public pour le comparer à l'Origin du navigateur.
7. Dans les paramètres supplémentaires de cette route, activez **Protect with Access**.
   Renseignez le nom de votre équipe Zero Trust et l'**Application Audience (AUD) Tag**
   de l'application Helm si demandés. Cela fait vérifier le jeton Access par `cloudflared`
   avant de transmettre la requête à Helm. Enregistrez la route.

Pour un domaine géré entièrement par Cloudflare, la route crée normalement le DNS nécessaire.
Ne créez pas d'enregistrement A vers l'adresse IP du serveur. Aucun port entrant Helm n'est
nécessaire ; le connecteur établit les connexions sortantes vers Cloudflare.

Références officielles : [tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/get-started/create-remote-tunnel/),
[application Access](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/self-hosted-public-app/),
[politiques](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/),
[One-time PIN](https://developers.cloudflare.com/cloudflare-one/integrations/identity-providers/one-time-pin/),
[Protect with Access](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/cloudflared-parameters/origin-parameters/#access).

## 4. Construire, démarrer et se connecter aux CLI

### Créer le jeton GitHub limité au dépôt cible

Dans GitHub, ouvrez Settings > Developer settings > Personal access tokens >
Fine-grained tokens > Generate new token :

1. Donnez-lui un nom, par exemple `Helm`, et une expiration courte adaptée à votre usage.
2. Choisissez comme Resource owner le propriétaire du dépôt cible. Si c'est une
   organisation, son approbation peut être nécessaire avant que le jeton fonctionne.
3. Dans Repository access, choisissez **Only select repositories** et sélectionnez
   **uniquement le dépôt cible**, par exemple `charlescstpierr/helm`.
4. Dans Repository permissions, accordez seulement **Contents: Read and write** pour
   les commits/push et la fusion, et **Pull requests: Read and write** pour les PR.
   **Metadata: Read-only** est ajouté automatiquement par GitHub. Ne donnez aucun droit
   d'administration ni permission de compte supplémentaire.
5. Générez le jeton et copiez-le dans `.env`, à la ligne `GH_TOKEN`. GitHub ne l'affiche
   qu'une fois. N'utilisez pas un jeton classique avec le scope global `repo`.

`gh` utilise directement `GH_TOKEN`. Le jeton n'est pas copié dans l'image et reste dans
le fichier `.env` protégé sur l'hôte. N'utilisez pas `gh auth login` par navigateur dans
ce déploiement : cette connexion OAuth ne serait pas limitée au seul dépôt cible.
Il n'est pas nécessaire d'enregistrer le PAT avec `gh auth login --with-token`.

Référence : [créer un jeton fine-grained](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#creating-a-fine-grained-personal-access-token).
La [documentation de gh](https://cli.github.com/manual/gh_auth_login) recommande `GH_TOKEN`
pour ces jetons. Leur portée doit être validée avec le dépôt réel ; la lecture des contrôles
CI peut demander des droits supplémentaires selon les contrôles utilisés. Un refus doit
rester visible, sans élargir automatiquement les permissions ni contourner une revue.
Les modifications aux fichiers de workflows GitHub Actions peuvent aussi être refusées
avec ces seuls droits.

### Démarrer et configurer Claude et Git

```sh
docker compose config --quiet
docker compose build
docker compose up -d
docker compose ps
docker compose logs --tail=100 helm cloudflared
```

Le journal Helm doit annoncer `127.0.0.1:7878` et `agents enabled`. Il ne doit pas annoncer
« aucune exécution d'agent ne sera lancée ». En cas d'erreur de dépôt, vérifiez le clone,
ses droits et `TARGET_REPO`. Le tunnel doit apparaître Healthy dans Cloudflare.

Si vous aviez déjà enregistré un ancien jeton dans le HOME persistant, révoquez-le dans
GitHub et retirez la connexion enregistrée, maintenant que le conteneur tourne :

```sh
docker compose exec helm env -u GH_TOKEN -u GITHUB_TOKEN gh auth logout --hostname github.com
```

Configurez ensuite Claude et Git une fois, avec le compte non root du conteneur :

```sh
docker compose exec helm claude
docker compose exec helm gh auth setup-git --hostname github.com
docker compose exec helm git config --global user.name "Votre nom"
docker compose exec helm git config --global user.email "votre-courriel@example.com"
docker compose exec helm gh auth status --hostname github.com
```

Pour Claude, suivez les indications du CLI ; ouvrez l'URL de connexion dans votre navigateur
sur votre ordinateur ou téléphone si le serveur ne peut pas l'ouvrir. Quittez ensuite la
session interactive Claude. GitHub utilise le PAT de `GH_TOKEN` ; `gh auth setup-git`
configure Git pour l'utiliser lors des opérations HTTPS. Aucun port ne doit être publié
pour ces connexions.

Les connexions, sessions Claude et réglages Git vivent dans le volume `helm_home`, sous
`/home/helm`, notamment `.claude`, `.claude.json`, `.config/gh` et `.gitconfig`. La base et
les worktrees ont leurs propres volumes. Le dépôt cible reste dans son dossier sur l'hôte.
Le nouveau jeton GitHub est fourni par `.env` à chaque création du conteneur, plutôt que
par une connexion OAuth sauvegardée dans `.config/gh`. Pour le renouveler, remplacez
`GH_TOKEN` dans `.env`, recréez les deux services et révoquez l'ancien jeton dans GitHub.

**Les agents partagent encore le HOME et le réseau de Helm, et héritent de `GH_TOKEN`.
Un PAT limité réduit les dépôts exposés, mais n'empêche pas un agent de lire ce jeton,
les identifiants Claude ou de demander une fusion par l'API locale. L'isolation des agents
reste un prérequis avant de s'appuyer sur l'étape des revues humaines.**

## 5. Vérifier l'accès

```sh
docker compose exec -T helm curl --fail http://127.0.0.1:7878/healthz
ss -tlnp 'sport = :7878'
```

Le premier appel doit afficher `ok`. Le second doit afficher uniquement les titres, sans
écoute sur le port 7878. `docker compose ps` ne doit afficher aucun port publié.

Ouvrez `https://helm.votre-domaine.com` dans une fenêtre privée. La connexion Cloudflare doit
apparaître avant le tableau. Connectez-vous avec votre courriel autorisé, puis créez une carte
**sans agent** pour vérifier les écritures. Avec un autre courriel, l'accès doit être refusé.
Répétez depuis le téléphone. Ne lancez une carte avec Claude qu'après ces vérifications.

Pour redémarrer les deux conteneurs en recréant leur espace réseau partagé :

```sh
docker compose up -d --force-recreate
docker compose exec -T helm gh auth status --hostname github.com
```

Vérifiez aussi que les cartes sont toujours là et que Claude reste connecté. Évitez de recréer
seulement `cloudflared` : Helm pourrait rester attaché à l'ancien espace réseau. Un redémarrage
automatique du connecteur mérite également un contrôle de l'accès ; recréez les deux services
ensemble si nécessaire. Gardez le même dossier et le même nom de projet Compose pour retrouver
les mêmes volumes. **N'utilisez pas `docker compose down -v` : cette option supprime les données
et les connexions persistantes.**

## 6. Mettre à jour et consulter les logs

Dans le clone qui contient les fichiers de déploiement, récupérez la version voulue. Par exemple,
après fusion de cette PR, avec un checkout propre sur `main` :

```sh
git pull --ff-only
docker compose pull cloudflared
docker compose build --pull
docker compose up -d --force-recreate
docker compose logs -f --tail=100 helm cloudflared
```

`Ctrl-C` quitte l'affichage des logs sans arrêter les services. La version de Claude Code est
fixée par `CLAUDE_CODE_VERSION` dans le Dockerfile. Pour la changer, modifiez cette valeur et
reconstruisez. Les volumes ne sont pas intégrés à l'image et survivent à sa reconstruction.
GitHub CLI vient du dépôt apt officiel `https://cli.github.com/packages`, vérifié avec sa
clé dédiée via `signed-by`. Sa version est épinglée par `GH_VERSION=2.102.0` dans le Dockerfile.
Pour la changer, modifiez ce numéro ainsi que la version attendue dans
`deploy/verify-gh.mjs`, puis reconstruisez et relancez la vérification locale.
L'image cloudflared est épinglée à `2026.10.0` dans `docker-compose.yml`. `pull` conserve
cette version ; pour la mettre à jour, modifiez explicitement ce tag après lecture des
notes de version, puis tirez l'image et recréez les deux services.

## 7. Sauvegarder la base

SQLite utilise WAL. Ne copiez pas simplement `helm.db` pendant que Helm fonctionne : des
écritures peuvent encore être dans `helm.db-wal`. Utilisez la sauvegarde SQLite intégrée :

```sh
mkdir -p backups
chmod 700 backups
docker compose exec -T helm sqlite3 /data/helm.db '.backup /data/helm-backup.db'
docker compose cp helm:/data/helm-backup.db ./backups/helm-backup.db
chmod 600 backups/helm-backup.db
```

Copiez cette sauvegarde vers un autre emplacement sûr. Sauvegardez aussi le clone dédié,
les worktrees et le HOME persistant si vous voulez retrouver le code et les connexions après
une perte du serveur. Le HOME contient des secrets et doit être protégé. Une sauvegarde de
la base seule ne sauvegarde ni le code des cartes ni les sessions et connexions des CLI.

## Vérification locale sans compte Cloudflare

```sh
bash deploy/verify-local.sh
```

Ce script construit la même image Helm et utilise un projet Compose temporaire avec des volumes
neufs. Il remplace le connecteur par un processus local qui garde l'espace réseau ouvert,
sans tunnel public. Il vérifie HTTP, les domaines, les écritures, l'absence de port publié,
Tini, les commandes et options de `src/github.rs` avec l'aide du vrai `gh`, les champs JSON,
les vérifications du clone Helm dans l'image et la persistance après redémarrage et
recréation. Il lance aussi le faux agent des tests pour vérifier qu'un SIGTERM permet à Helm
d'enregistrer l'interruption avant tout redémarrage. Il supprime ensuite ses
propres conteneurs et volumes. Il ne touche pas au déploiement existant.

**Ce test ne prouve ni la connexion d'un tunnel réel, ni les règles Access, ni la validité ou
la persistance de vraies connexions Claude/GitHub.** Les marqueurs qu'il place dans le HOME
prouvent la persistance des fichiers, pas celle d'une authentification. Ces derniers points
doivent être vérifiés après vos connexions réelles.
