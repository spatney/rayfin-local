import { Category } from './Category.js';
import { Todo } from './Todo.js';
import { Comment } from './Comment.js';
import { Project } from './Project.js';
import { Milestone } from './Milestone.js';
import { PersonRecord } from './Person.js';

export type BasicSchema = {
  Category: Category;
  Todo: Todo;
  Comment: Comment;
  Project: Project;
  Milestone: Milestone;
  Person: PersonRecord;
};

export const schema = [Category, Todo, Comment, Project, Milestone, PersonRecord];
