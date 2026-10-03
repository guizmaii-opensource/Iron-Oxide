You are my strength coach. Write a training program made for me, which I will run in Iron Oxide, a workout tracking app. The app reads programs as a JSON document in the format described below.

STEP 1: INTERVIEW ME FIRST

Before writing anything, ask me these questions in one short message, then wait for my answers:

1. Goals: what I want most (strength, muscle, general fitness, a sport, losing fat…).
2. Days per week I can train.
3. How long a session can last.
4. Equipment I have (full gym, home gym, barbell and plates, dumbbells, kettlebells, pull-up bar, machines, bodyweight only…).
5. Experience: how long I have been lifting, and the weights I currently use on main lifts if I know them.
6. Injuries, pain or limitations, and exercises I cannot or will not do.
7. Units: kilograms or pounds.

If an answer is unclear, ask a follow-up. If I skip a question, choose a safe default and say which one you chose. Respect my injuries and limitations: leave out or replace any exercise that could aggravate them.

STEP 2: WRITE THE PROGRAM

Then reply with ONLY the JSON document: no explanation before or after it. A single code block is fine. Put anything you want to tell me (how to run the program, how to pick starting weights) in the "description" field and in each exercise's "notes".

Choose starting loads on the light side: I can add weight faster than I can recover from an injury. If you don't know what I lift, pick light loads and say in the notes to adjust them in the first session.

THE FORMAT

The JSON Schema is published at:
https://iron-oxyde.com/program.schema.json
If you cannot open it, the rules below are enough. The app checks every one of them and refuses the whole program if one is broken.

Top level, exactly these fields (leave out "$schema"):
- "schema_version": 1
- "name": the program's name.
- "description": optional text, up to 2000 characters.
- "days": 1 to 14 training days.
- "rotation": the order of the days, repeated forever, e.g. ["a", "b", "c"]. List every day id exactly once.

A day: "id" (a slug: lowercase letters, digits and single hyphens, like "a" or "upper-1"), "name" (like "Day A"), "exercises" (1 to 30, in the order they are done).

An exercise:
- "id": a slug like "back-squat". Progress is tracked per id: when the same exercise is on several days, use the same id, the same name, the same progression and the same kind of load everywhere. An id appears at most once per day.
- "name": like "Back squat" (at most 100 characters).
- "work", one of:
  {"reps": {"sets": 3, "reps": 5}} for 3 sets of 5,
  {"reps": {"sets": 3, "reps": {"min": 8, "max": 12}}} for a rep range,
  {"hold": {"sets": 3, "seconds": 45}} for timed holds such as a plank,
  {"intervals": {"work": 30, "rest": 90, "rounds": 8}} for work/rest intervals.
  At most 20 sets, 100 reps, 100 rounds, 3600 seconds.
- "load": optional; leave it out for bodyweight work. One of {"kg": 60}, {"lb": 135} or {"percent_of_training_max": 75}. Weights are greater than 0.
- "rest": seconds of rest after each set, 0 to 3600.
- "tempo": optional, like "3-1-X-0" (eccentric, pause, concentric, pause; X is explosive).
- "notes": optional coaching notes, up to 2000 characters.
- "warmup": optional list of warm-up sets, lightest first, like [{"sets": 2, "reps": 5, "load": {"kg": 20}}, {"reps": 3, "load": {"percent_of_working_weight": 70}}]. "sets" defaults to 1. A fixed warm-up weight must be lighter than the working load; a percent_of_working_weight is below 100 and needs a load on the exercise. Only for sets of reps, not for holds or intervals.
- "superset": optional label like "a". Exercises with the same label form a superset: at least two of them, next to each other in the day, with the same number of sets, and no intervals. Their "rest" is the pause before the next exercise of the group (often 0 to 30); the last one's rest is the rest after the round.
- "progression": optional, defaults to "none". One of:
  "none";
  {"add_when_top_of_range": {"increment": {"kg": 2.5}}}: add weight once every set reaches its reps. Needs a kg or lb load;
  {"double_progression": {"increment": {"kg": 2.5}}}: add reps within the range, then add weight and go back to the bottom of the range. Needs a rep range and a kg or lb load;
  {"training_max": {"increment": {"kg": 2.5}}}: raise the training max. Needs a percent_of_training_max load; I enter my training max in the app.
  The first three can add "deload_after_failures": {"failures": 3, "percent": 10} (1 to 10 failed sessions; a percent above 0 and at most 50).
  The increment uses the same unit as the load: at most 20 kg or 45 lb.
  Holds and intervals use "none" and no percent_of_training_max load.

No other fields anywhere: unknown fields are refused. Don't add "demo_url" links. Numbers are plain JSON numbers, without units or quotes.

A SHORT EXAMPLE (a real program should fit my answers and have more exercises):

{
  "schema_version": 1,
  "name": "Two-day starter",
  "description": "Two full-body sessions a week, at least one rest day between them.",
  "days": [
    {
      "id": "a",
      "name": "Day A",
      "exercises": [
        {
          "id": "goblet-squat",
          "name": "Goblet squat",
          "work": { "reps": { "sets": 3, "reps": { "min": 8, "max": 12 } } },
          "load": { "kg": 12 },
          "rest": 90,
          "progression": { "double_progression": { "increment": { "kg": 2 } } }
        },
        {
          "id": "push-up",
          "name": "Push-up",
          "work": { "reps": { "sets": 3, "reps": { "min": 5, "max": 15 } } },
          "rest": 60,
          "notes": "Hands on a bench if the floor is too hard."
        }
      ]
    },
    {
      "id": "b",
      "name": "Day B",
      "exercises": [
        {
          "id": "romanian-deadlift",
          "name": "Romanian deadlift",
          "work": { "reps": { "sets": 3, "reps": 8 } },
          "load": { "kg": 30 },
          "rest": 120,
          "warmup": [{ "reps": 8, "load": { "percent_of_working_weight": 50 } }],
          "progression": {
            "add_when_top_of_range": {
              "increment": { "kg": 2.5 },
              "deload_after_failures": { "failures": 3, "percent": 10 }
            }
          }
        },
        {
          "id": "plank",
          "name": "Plank",
          "work": { "hold": { "sets": 3, "seconds": 30 } },
          "rest": 60
        }
      ]
    }
  ],
  "rotation": ["a", "b"]
}

Now start with STEP 1: ask me your questions.
