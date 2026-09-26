import { defineJourney } from '@dorsk/journey';

// Cards and message lines repeat, so a bare path matches every one of them and
// throws. `nth` indexes the visible matches and exists only on the locator form.
const first = (path: string[]) =>
	({ css: path.map((n) => `[data-journey="${n}"]`).join(' '), nth: 0 }) as const;
const FIRST_SESSION_TITLE = first(['session', 'title']);
const FIRST_LINE = first(['conversation', 'line']);
const FIRST_LINE_ACTIONS = first(['conversation', 'line', 'line-actions']);

export default defineJourney({
	id: 'follow-session',
	title: { en: 'Follow a session while it works', fr: 'Suivre une session pendant son travail' },
	description: { en: 'Open a running agent, read what it did, and reply without leaving the list.', fr: 'Ouvrez un agent en cours, lisez ce qu’il a fait et répondez sans quitter la liste.' },
	route: '/sessions',
	fixture: 'instance',
	variants: { viewport: ['desktop', 'mobile'], theme: ['dark'] },
	level: 'checked',
	steps: [
		{
			id: 'open',
			route: '/sessions',
			// Not a session id snapshotted at start: it is gone the moment that
			// session ends, is archived, or scrolls out of view.
			target: FIRST_SESSION_TITLE,
			do: { kind: 'click' },
			say: {
				title: { en: 'Open a session', fr: 'Ouvrir une session' },
				body: {
					en: 'Open one by its name. The conversation slides in beside the list on a desktop and over it on a phone, so you never lose your place in the fleet.',
					fr: 'Ouvrez-en une par son nom. La conversation s’ouvre à côté de la liste sur un ordinateur, par-dessus sur un téléphone : vous ne perdez jamais votre place dans la flotte.'
				}
			},
			expect: [{ visible: 'conversation' }, { visible: 'composer' }],
			capture: 'drawer'
		},
		{
			id: 'header',
			target: 'conversation/header',
			say: {
				title: { en: 'Who is running this, and where', fr: 'Qui exécute ceci, et où' },
				body: {
					en: 'The top row answers the questions you ask first: is it alive, which machine is it on, which account is paying for it, and what is it called.',
					fr: 'La première ligne répond aux questions qu’on se pose d’abord : est-elle vivante, sur quelle machine tourne-t-elle, quel compte la paie, et comment s’appelle-t-elle.'
				}
			},
			expect: [{ visible: 'conversation/header' }],
			capture: 'header'
		},
		{
			id: 'meta',
			target: 'conversation/head-meta',
			say: {
				title: { en: 'What it is working on, and what it has spent', fr: 'Sur quoi elle travaille, et ce qu’elle a dépensé' },
				body: {
					en: 'The second row is the run’s cost and context: working directory, git branch, model, and the token usage so far. Watch it when a session starts feeling slow or expensive.',
					fr: 'La seconde ligne donne le coût et le contexte : répertoire de travail, branche git, modèle et jetons consommés. Surveillez-la quand une session devient lente ou coûteuse.'
				}
			},
			expect: [{ visible: 'conversation/head-meta' }]
		},
		{
			id: 'actions',
			when: { viewport: 'desktop' },
			target: 'conversation/actions',
			say: {
				title: { en: 'Branch instead of starting over', fr: 'Bifurquer plutôt que tout recommencer' },
				body: {
					en: 'The ⋯ menu holds the less-used actions: fork, which copies the history up to a message and continues from there — how you try a second approach without losing the first — plus copy a link, export, and the read-only live terminal.',
					fr: 'Le menu ⋯ regroupe les actions moins courantes : bifurquer, qui copie l’historique jusqu’à un message et repart de là — pour tenter une seconde approche sans perdre la première —, copier un lien, exporter et le terminal en direct en lecture seule.'
				}
			},
			expect: [{ visible: 'conversation/actions' }]
		},
		{
			id: 'kinds',
			target: FIRST_LINE,
			say: {
				title: { en: 'Everything it did is on the record', fr: 'Tout ce qu’elle a fait est consigné' },
				body: {
					en: 'Each message is badged with its kind: your prompts, the agent’s replies, its reasoning, every tool call and the result that came back. Nothing is summarised away.',
					fr: 'Chaque message porte son type : vos prompts, les réponses de l’agent, son raisonnement, chaque appel d’outil et le résultat renvoyé. Rien n’est résumé ni masqué.'
				}
			},
			expect: [{ visible: FIRST_LINE }],
			capture: 'timeline'
		},
		{
			id: 'line-actions',
			target: FIRST_LINE_ACTIONS,
			say: {
				title: { en: 'Lift one message out', fr: 'Extraire un message' },
				body: {
					en: 'Any single message can be pinned to find again, copied as Markdown for a ticket, or saved as an image to paste into a review.',
					fr: 'N’importe quel message peut être épinglé pour le retrouver, copié en Markdown pour un ticket, ou enregistré en image à coller dans une revue.'
				}
			},
			expect: [{ visible: FIRST_LINE_ACTIONS }],
			capture: 'line'
		},
		{
			id: 'filters',
			target: 'filters',
			say: {
				title: { en: 'Hide the noise', fr: 'Masquer le bruit' },
				body: {
					en: 'These pills hide whole kinds of message. Turning the assistant off leaves only the tool calls — the quickest way to see what an agent actually touched.',
					fr: 'Ces pastilles masquent des types entiers de messages. Désactiver l’assistant ne laisse que les appels d’outils — le moyen le plus rapide de voir ce que l’agent a réellement touché.'
				}
			},
			expect: [{ visible: 'filters/quick[assistant]' }]
		},
		{
			id: 'filter-menu',
			target: 'filters/filter-menu',
			say: {
				title: { en: 'Or pick the categories yourself', fr: 'Ou choisir les catégories vous-même' },
				body: {
					en: 'The pills are shortcuts over a finer list. Open it when you want one tool kind and nothing else — reading only the file writes, for instance.',
					fr: 'Les pastilles sont des raccourcis sur une liste plus fine. Ouvrez-la pour ne garder qu’un seul type d’outil — les écritures de fichiers, par exemple.'
				}
			},
			expect: [{ visible: 'filters/filter-menu' }]
		},
		{
			id: 'tools-only',
			qaOnly: true,
			target: 'filters/quick[assistant]',
			do: { kind: 'click' },
			say: {
				title: { en: 'Hide the noise', fr: 'Masquer le bruit' },
				body: {
					en: 'Hiding the assistant messages leaves the tool calls — the fastest way to audit what an agent touched.',
					fr: 'Masquer les messages de l’assistant ne laisse que les appels d’outils — le moyen le plus rapide d’auditer ce que l’agent a touché.'
				}
			},
			expect: [{ visible: 'conversation/line[tool]' }, { hidden: 'conversation/line[assistant]' }],
			capture: 'tools'
		},
		{
			id: 'reply',
			target: 'composer/message',
			say: {
				title: { en: 'Steer it from here', fr: 'La piloter d’ici' },
				body: {
					en: 'Anything you type goes to the running agent, so you can redirect it mid-task instead of stopping it and starting again.',
					fr: 'Ce que vous tapez part vers l’agent en cours : vous pouvez le réorienter en pleine tâche au lieu de l’arrêter et de recommencer.'
				}
			},
			expect: [{ visible: 'composer/message' }],
			capture: 'reply'
		}
	]
});
